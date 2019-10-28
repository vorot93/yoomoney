mod models;
mod transport;

pub use models::*;
pub use transport::*;

use async_stream::try_stream;
use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::prelude::*;
use itertools::*;
use maplit::hashmap;
use phonenumber::PhoneNumber;
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    future::Future,
    pin::Pin,
    sync::Arc,
};
use tokio_stream::*;
use uuid::Uuid;

#[async_trait]
pub trait API {
    async fn account_info(&self) -> anyhow::Result<AccountInfo>;
    fn operation_history(
        &self,
        operation_types: HashSet<ReqOperationType>,
        label: Option<String>,
        from: Option<DateTime<Utc>>,
        till: Option<DateTime<Utc>>,
        start_record: u64,
        details: bool,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<Operation>> + Send>>;
    async fn operation_details(&self, operation_id: String) -> anyhow::Result<OperationDetails>;
    fn request_shop_payment(
        &self,
        pattern_id: String,
        other: HashMap<String, String>,
    ) -> PaymentRequest;
    #[allow(clippy::too_many_arguments)]
    fn request_transfer(
        &self,
        to: UserId,
        amount: RequestAmount,
        comment: String,
        message: String,
        label: Option<String>,
        codepro: bool,
        hold_for_pickup: bool,
        expire_period: u32,
    ) -> PaymentRequest;
    fn request_mobile_payment(
        &self,
        phone_number: PhoneNumber,
        amount: BigDecimal,
    ) -> PaymentRequest;
    async fn process_payment(
        &self,
        request_id: String,
        money_source: ProcessPaymentMoneySource,
    ) -> anyhow::Result<ProcessPaymentResponse>;
}

#[async_trait]
pub trait PaymentRequestTrait {
    async fn send(self) -> anyhow::Result<RequestPaymentResponse>;
}

pub struct PaymentRequest {
    caller: CallerWrapper,
    params: HashMap<String, String>,
}

#[async_trait]
impl PaymentRequestTrait for PaymentRequest {
    async fn send(self) -> anyhow::Result<RequestPaymentResponse> {
        let params = self
            .params
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        self.caller
            .call("api/request-payment", &params)
            .await?
            .into_result()
    }
}

pub struct TestPaymentRequest {
    inner: PaymentRequest,
}

impl From<PaymentRequest> for TestPaymentRequest {
    fn from(inner: PaymentRequest) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl PaymentRequestTrait for TestPaymentRequest {
    async fn send(mut self) -> anyhow::Result<RequestPaymentResponse> {
        self.inner
            .params
            .insert("test_payment".to_string(), true.to_string());

        Ok(self.inner.send().await?)
    }
}

pub struct Client {
    caller: CallerWrapper,
}

impl Client {
    pub fn new<T: Display>(token: Option<T>) -> Self {
        let http_client = reqwest::Client::builder().build().unwrap();
        Self {
            caller: CallerWrapper {
                transport: Arc::new(RemoteCaller {
                    http_client,
                    addr: "https://money.yandex.ru".into(),
                    bearer: token.map(|t| t.to_string()),
                }),
            },
        }
    }

    pub async fn revoke_token(self) -> anyhow::Result<()> {
        self.caller
            .call_empty("api/revoke", &Default::default())
            .await
    }
}

pub struct UnauthorizedClient {
    caller: CallerWrapper,
    client_id: String,
    redirect_uri: String,
}

impl UnauthorizedClient {
    #[must_use]
    pub fn new(client_id: String, redirect_uri: String) -> Self {
        let http_client = reqwest::Client::builder().build().unwrap();
        Self {
            caller: CallerWrapper {
                transport: Arc::new(RemoteCaller {
                    http_client,
                    addr: "https://money.yandex.ru".into(),
                    bearer: None,
                }),
            },
            client_id,
            redirect_uri,
        }
    }

    pub async fn authorize<F, Fut>(
        self,
        access_scope: HashSet<AccessScope>,
        authorize_callback: F,
    ) -> anyhow::Result<String>
    where
        F: Fn(String) -> Fut + Send,
        Fut: Future<Output = anyhow::Result<String>> + Send,
    {
        // Get address to be opened in browser
        let redirect_addr = self
            .caller
            .get_redirect(
                "oauth/authorize",
                &hashmap! {
                    "client_id" => self.client_id.clone(),
                    "response_type" => "code".to_string(),
                    "redirect_uri" => self.redirect_uri.clone(),
                    "scope" => access_scope.iter().map(|s| ron::ser::to_string(s).unwrap()).join(" "),
                    "instance_name" => Uuid::new_v4().to_string(),
                },
            )
            .await?;

        // This should open the page in browser
        let temp_token_fut = authorize_callback(redirect_addr);
        let temp_token = temp_token_fut.await?;

        let token = self
            .caller
            .call::<TokenExchangeData>(
                "oauth/token",
                &hashmap! {
                    "code" => temp_token,
                    "client_id" => self.client_id.clone(),
                    "grant_type" => "authorization_code".into(),
                    "redirect_uri" => self.redirect_uri.clone(),
                },
            )
            .await?
            .into_result()?;

        Ok(token.access_token)
    }
}

#[async_trait]
impl API for Client {
    async fn account_info(&self) -> anyhow::Result<AccountInfo> {
        Ok(self
            .caller
            .call("api/account-info", &Default::default())
            .await?
            .into_result()?)
    }

    fn operation_history(
        &self,
        operation_types: HashSet<ReqOperationType>,
        label: Option<String>,
        from: Option<DateTime<Utc>>,
        till: Option<DateTime<Utc>>,
        mut start_record: u64,
        details: bool,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<Operation>> + Send>> {
        let caller = self.caller.clone();
        let mut params = HashMap::new();
        params.insert(
            "types",
            operation_types
                .iter()
                .map(|v| serde_json::to_string(v).unwrap())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if let Some(label) = label {
            params.insert("label", label);
        }
        if let Some(v) = from {
            params.insert("from", v.to_rfc3339());
        }
        if let Some(v) = till {
            params.insert("till", v.to_rfc3339());
        }
        params.insert("details", details.to_string());

        Box::pin(try_stream! {
            loop {
                params.insert("start-record", start_record.to_string());

                let rsp = caller
                    .call::<OperationHistoryResponse>("api/operation-history", &params)
                    .await?;

                    let rsp = rsp.into_result()?;

                if rsp.operations.is_empty() {
                    return;
                }

                for op in rsp.operations {
                    yield op;
                }

                match rsp.next_record {
                    Some(v) => {
                        start_record = v.0;
                    }
                    None => {
                        return;
                    }
                }
            }
        })
    }

    async fn operation_details(&self, operation_id: String) -> anyhow::Result<OperationDetails> {
        Ok(self
            .caller
            .call(
                "api/operation-details",
                &hashmap! { "operation_id" => operation_id },
            )
            .await?
            .into_result()?)
    }

    fn request_shop_payment(
        &self,
        pattern_id: String,
        other: HashMap<String, String>,
    ) -> PaymentRequest {
        let mut params = HashMap::new();
        params.insert("pattern_id".to_string(), pattern_id);
        for (k, v) in other {
            params.insert(k, v);
        }

        PaymentRequest {
            caller: self.caller.clone(),
            params,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn request_transfer(
        &self,
        to: UserId,
        amount: RequestAmount,
        comment: String,
        message: String,
        label: Option<String>,
        codepro: bool,
        hold_for_pickup: bool,
        expire_period: u32,
    ) -> PaymentRequest {
        let mut params = hashmap! {
            "pattern_id" => "p2p".into(),
            "to" => to.to_string(),
            "comment" => comment,
            "message" => message,
            "codepro" => codepro.to_string(),
            "hold_for_pickup" => hold_for_pickup.to_string(),
            "expire_period" => expire_period.to_string(),
        };

        match amount {
            RequestAmount::Total(amount) => {
                params.insert("amount", amount.to_string());
            }
            RequestAmount::Net(amount_due) => {
                params.insert("amount_due", amount_due.to_string());
            }
        }

        if let Some(v) = label {
            params.insert("label", v);
        }

        PaymentRequest {
            caller: self.caller.clone(),
            params: params
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    fn request_mobile_payment(
        &self,
        phone_number: PhoneNumber,
        amount: BigDecimal,
    ) -> PaymentRequest {
        let params = hashmap! {
            "pattern_id" => "phone-topup".to_string(),
            "phone-number" => phone_number.to_string(),
            "amount" => amount.to_string(),
        };

        PaymentRequest {
            caller: self.caller.clone(),
            params: params
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    async fn process_payment(
        &self,
        request_id: String,
        money_source: ProcessPaymentMoneySource,
    ) -> anyhow::Result<ProcessPaymentResponse> {
        let mut params = HashMap::new();
        params.insert("request_id", request_id);
        match money_source {
            ProcessPaymentMoneySource::Wallet => {
                params.insert("money_source", "wallet".into());
            }
            ProcessPaymentMoneySource::Card { id, secure3d } => {
                params.insert("money_source", id);
                if let Some(data) = secure3d {
                    params.insert("ext_auth_success_uri", data.ext_auth_success_uri);
                    params.insert("ext_auth_fail_uri", data.ext_auth_fail_uri);
                }
            }
        }

        Ok(self
            .caller
            .call("api/process-payment", &params)
            .await?
            .into_result()?)
    }
}
