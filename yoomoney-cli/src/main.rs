use anyhow::format_err;
use bigdecimal::*;
use chrono::prelude::*;
use clap::*;
use phonenumber::*;
use serde::*;
use std::{path::*, str::FromStr};
use tokio_stream::*;
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;
use yoomoney::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Config {
    token: String,
}

fn config_location() -> PathBuf {
    let mut path = xdg::BaseDirectories::new().unwrap().get_config_home();
    path.push("yandex-money-cli/config.toml");

    path
}

#[derive(Debug, Parser)]
struct AuthorizeData {
    #[clap(long, env = "CLIENT_ID")]
    client_id: String,
    #[clap(long, env = "CLIENT_REDIRECT")]
    client_redirect: String,
    #[clap(short)]
    do_not_store_on_disk: bool,
}

#[derive(Debug, Parser)]
enum UnauthorizedCmd {
    /// Authorize client
    Login(AuthorizeData),
}

#[derive(Debug, Parser)]
struct To {
    #[clap(long, conflicts_with_all = &["to-email", "to-phone"])]
    to_account: Option<u64>,
    #[clap(long, conflicts_with_all = &["to-account", "to-phone"])]
    to_email: Option<String>,
    #[clap(long, conflicts_with_all = &["to-account", "to-email"])]
    to_phone: Option<PhoneNumber>,
}

impl From<To> for Option<UserId> {
    fn from(value: To) -> Self {
        if let Some(v) = value.to_account {
            return Some(UserId::Account(v));
        }

        if let Some(v) = value.to_email {
            return Some(UserId::Email(v));
        }

        if let Some(v) = value.to_phone {
            return Some(UserId::Phone(v));
        }

        None
    }
}

#[derive(Debug, Parser)]
struct Amount {
    #[clap(long, conflicts_with = "amount-total")]
    amount_net: Option<BigDecimal>,
    #[clap(long, conflicts_with = "amount-net")]
    amount_total: Option<BigDecimal>,
}

impl From<Amount> for Option<RequestAmount> {
    fn from(value: Amount) -> Self {
        if let Some(v) = value.amount_net {
            return Some(RequestAmount::Net(v));
        }

        if let Some(v) = value.amount_total {
            return Some(RequestAmount::Total(v));
        }

        None
    }
}

#[derive(Debug, Parser)]
#[allow(clippy::large_enum_variant)]
enum AuthorizedCmd {
    /// Reauthorize client
    Login(AuthorizeData),
    /// Revoke token
    Revoke,
    /// Request transfer
    RequestTransfer {
        #[clap(flatten)]
        to: To,
        #[clap(flatten)]
        amount: Amount,
        #[clap(long)]
        comment: Option<String>,
        #[clap(long)]
        message: Option<String>,
        #[clap(long)]
        label: Option<String>,
        #[clap(long)]
        codepro: Option<bool>,
        #[clap(long)]
        hold_for_pickup: Option<bool>,
        #[clap(long)]
        expire_period: Option<u32>,
    },
    /// Process existing payment
    ProcessPayment {
        #[clap(long)]
        request_id: String,
        #[clap(long)]
        money_source: ProcessPaymentMoneySource,
    },
    /// Show operation history
    OperationHistory {
        #[clap(long)]
        from: Option<DateTime<Utc>>,
        #[clap(long)]
        till: Option<DateTime<Utc>>,
        #[clap(long)]
        detailed: bool,
    },
}

async fn do_authorize(
    AuthorizeData {
        client_id,
        client_redirect,
        do_not_store_on_disk,
    }: AuthorizeData,
) -> anyhow::Result<()> {
    let client = UnauthorizedClient::new(client_id, client_redirect);

    let permanent_token = client
        .authorize(
            vec![
                AccessScope::AccountInfo,
                AccessScope::OperationHistory,
                AccessScope::PaymentP2P,
            ]
            .into_iter()
            .collect(),
            |redirect_addr| async move {
                println!("Please open this page in your browser: {redirect_addr}");
                println!("Copy and paste your redirect URI here");

                let mut stdin = tokio_util::codec::FramedRead::new(
                    tokio::io::stdin(),
                    tokio_util::codec::LinesCodec::new(),
                );
                let uri = stdin.next().await.unwrap().unwrap();

                let uri = Url::from_str(&uri.replace('\n', ""))?;

                let token = uri
                    .query_pairs()
                    .find_map(|(key, value)| {
                        if *key == *"code" {
                            Some(value.to_string())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| format_err!("Authorization code not found in redirect URL"))?;

                println!("Extracted token: {token}");

                Ok(token)
            },
        )
        .await?;

    if !do_not_store_on_disk {
        let path = config_location();
        println!("Saving token on disk to {}", path.to_string_lossy());
        let _ = std::fs::create_dir_all(&path);
        tokio::fs::write(
            path,
            toml::to_string(&Config {
                token: permanent_token.clone(),
            })
            .unwrap(),
        )
        .await
        .unwrap();
    }

    println!("Your permanent token is {permanent_token:?}");

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::from_default_env();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    let token = match std::env::var("TOKEN").ok() {
        Some(v) => Some(v),
        None => {
            async move {
                if let Ok(data) = tokio::fs::read(config_location()).await {
                    if let Ok(s) = String::from_utf8(data) {
                        if let Ok(config) = toml::from_str::<Config>(&s) {
                            return Some(config.token);
                        }
                    }
                }

                None
            }
            .await
        }
    };

    match token {
        None => match UnauthorizedCmd::parse() {
            UnauthorizedCmd::Login(data) => do_authorize(data).await?,
        },
        Some(token) => match AuthorizedCmd::parse() {
            AuthorizedCmd::Login(data) => do_authorize(data).await?,
            other => {
                println!("Using token {token}");
                let client = Client::new(Some(token.clone()));
                match other {
                    AuthorizedCmd::Revoke => {
                        client.revoke_token().await?;
                        println!("Token {token} successfully revoked");
                    }
                    AuthorizedCmd::RequestTransfer {
                        to,
                        amount,
                        comment,
                        message,
                        label,
                        codepro,
                        hold_for_pickup,
                        expire_period,
                    } => {
                        let to =
                            Option::from(to).ok_or_else(|| format_err!("User ID not specified"))?;
                        let amount = Option::from(amount)
                            .ok_or_else(|| format_err!("Transfer amount not specified"))?;

                        let payment_request = client.request_transfer(
                            to,
                            amount,
                            comment.unwrap_or_default(),
                            message.unwrap_or_default(),
                            label,
                            codepro.unwrap_or_default(),
                            hold_for_pickup.unwrap_or_default(),
                            expire_period.unwrap_or_default(),
                        );

                        let res = payment_request.send().await;

                        println!("Payment request result is {res:?}");
                    }
                    AuthorizedCmd::OperationHistory {
                        detailed,
                        from,
                        till,
                    } => {
                        let mut history = client.operation_history(
                            Default::default(),
                            None,
                            from,
                            till,
                            0,
                            detailed,
                        );

                        while let Some(v) = history.next().await.transpose()? {
                            println!("{v:?}");
                        }
                    }
                    other => unimplemented!("{:?}", other),
                }
            }
        },
    };

    Ok(())
}
