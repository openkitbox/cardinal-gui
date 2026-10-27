//! Pure parser for the account export produced by Kiro's CLI.
//!
//! The export is intentionally not the same shape as [`gateway::kiro::KiroOAuth`].
//! The parser treats a missing, non-string, or blank `profileArn` as absent;
//! only a non-blank string is retained in the stored credential.

use std::fmt;

use gateway::kiro::{KiroOAuth, valid_region};
use serde::Deserialize;

const MIN_MILLISECOND_EPOCH: i64 = 1_000_000_000_000;

/// A parsed export. Its private fields keep the credential whitelist and the
/// source document out of callers that only need account selection UI.
pub struct ParsedKiroExport {
    accounts: Vec<ValidatedAccount>,
}

/// Secret-free information suitable for an account picker.
#[derive(Clone, Debug)]
pub struct AccountSummary {
    pub index: usize,
    pub region: String,
    pub expires_at: i64,
    pub has_profile_arn: bool,
}

/// A fixed, secret-free parse error. The parser never includes a field value
/// in an error, so a token cannot reach a GUI error card accidentally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    InvalidDocument,
    AccountsMissing,
    AccountsEmpty,
    AccountInvalid {
        index: usize,
        reason: InvalidAccountReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidAccountReason {
    CredentialsMissing,
    CredentialMissing(&'static str),
    CredentialEmpty(&'static str),
    ClientBundleIncomplete,
    AuthMethodUnsupported,
    AuthMethodMismatch,
    ExpiryMissing,
    ExpiryOutOfRange,
    RegionInvalid,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDocument => formatter.write_str("Kiro export must be a JSON object"),
            Self::AccountsMissing => formatter.write_str("Kiro export has no accounts array"),
            Self::AccountsEmpty => formatter.write_str("Kiro export accounts array is empty"),
            Self::AccountInvalid { index, reason } => {
                write!(formatter, "Kiro account {index} is invalid: {reason}")
            }
        }
    }
}

impl fmt::Display for InvalidAccountReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::CredentialsMissing => "credentials object is missing",
            Self::CredentialMissing(field) => match *field {
                "clientId" => "clientId is missing",
                "clientSecret" => "clientSecret is missing",
                "refreshToken" => "refreshToken is missing",
                "accessToken" => "accessToken is missing",
                "region" => "region is missing",
                _ => "credential field is missing",
            },
            Self::CredentialEmpty(field) => match *field {
                "clientId" => "clientId is empty",
                "clientSecret" => "clientSecret is empty",
                "refreshToken" => "refreshToken is empty",
                "accessToken" => "accessToken is empty",
                "region" => "region is empty",
                _ => "credential field is empty",
            },
            Self::ClientBundleIncomplete => "clientId and clientSecret must be provided together",
            Self::AuthMethodUnsupported => "authMethod is not supported",
            Self::AuthMethodMismatch => "authMethod does not match the supplied credential fields",
            Self::ExpiryMissing => "expiresAt is missing",
            Self::ExpiryOutOfRange => "expiresAt is invalid",
            Self::RegionInvalid => "region is not a valid AWS region",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ParseError {}

/// Parse and validate an export without retaining its raw JSON text.
pub fn parse(input: &str) -> Result<ParsedKiroExport, ParseError> {
    let document: serde_json::Value =
        serde_json::from_str(input).map_err(|_| ParseError::InvalidDocument)?;
    let accounts = match document {
        serde_json::Value::Object(root) if root.contains_key("accounts") => {
            let document: Export = serde_json::from_value(serde_json::Value::Object(root))
                .map_err(|_| ParseError::InvalidDocument)?;
            document.accounts.ok_or(ParseError::AccountsMissing)?
        }
        serde_json::Value::Object(root) => vec![Account {
            profile_arn: root.get("profileArn").cloned(),
            credentials: serde_json::from_value(serde_json::Value::Object(root))
                .map(Some)
                .map_err(|_| ParseError::InvalidDocument)?,
        }],
        _ => return Err(ParseError::InvalidDocument),
    };
    if accounts.is_empty() {
        return Err(ParseError::AccountsEmpty);
    }

    let accounts = accounts
        .into_iter()
        .enumerate()
        .map(|(index, account)| validate_account(index, account))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ParsedKiroExport { accounts })
}

/// Save-time parse boundary. Keeping this named separately makes it explicit
/// that the current textarea contents, rather than a stale UI snapshot, are
/// the source of the credential sent to the gateway.
pub fn parse_for_save(input: &str) -> Result<ParsedKiroExport, ParseError> {
    parse(input)
}

pub fn validate_import_fields(
    provider_name: &str,
    label: &str,
) -> Result<(String, String), &'static str> {
    let provider_name = provider_name.trim();
    let label = label.trim();
    if provider_name.is_empty() || provider_name.contains('/') {
        return Err("Provider 名称不能为空且不能包含 /");
    }
    if label.is_empty() {
        return Err("label 不能为空");
    }
    Ok((provider_name.to_owned(), label.to_owned()))
}

impl ParsedKiroExport {
    /// Return only non-secret account metadata for the picker.
    pub fn summaries(&self) -> impl Iterator<Item = AccountSummary> + '_ {
        self.accounts
            .iter()
            .enumerate()
            .map(|(index, account)| AccountSummary {
                index,
                region: account.credentials.region.clone(),
                expires_at: account.expires_at,
                has_profile_arn: account.profile_arn.is_some(),
            })
    }

    /// Convert one selected account into the gateway's stored credential shape.
    pub fn oauth(&self, index: usize) -> Option<KiroOAuth> {
        let account = self.accounts.get(index)?;
        Some(KiroOAuth {
            access_token: account.credentials.access_token.clone(),
            expires_at: account.expires_at,
            refresh_token: account.credentials.refresh_token.clone(),
            client_id: account.credentials.client_id.clone(),
            client_secret: account.credentials.client_secret.clone(),
            region: account.credentials.region.clone(),
            auth_region: account.credentials.auth_region.clone(),
            api_region: account.credentials.api_region.clone(),
            profile_arn: account.profile_arn.clone(),
        })
    }
}

fn validate_account(index: usize, account: Account) -> Result<ValidatedAccount, ParseError> {
    let credentials = account.credentials.ok_or(ParseError::AccountInvalid {
        index,
        reason: InvalidAccountReason::CredentialsMissing,
    })?;
    let client_id = credentials.client_id.unwrap_or_default();
    let client_secret = credentials.client_secret.unwrap_or_default();
    let has_client_id = !client_id.trim().is_empty();
    let has_client_secret = !client_secret.trim().is_empty();
    if has_client_id != has_client_secret {
        return Err(ParseError::AccountInvalid {
            index,
            reason: InvalidAccountReason::ClientBundleIncomplete,
        });
    }
    let idc = has_client_id && has_client_secret;
    if let Some(auth_method) = credentials.auth_method.as_deref() {
        let explicit_idc = match auth_method.to_ascii_lowercase().as_str() {
            "social" => false,
            "idc" | "builder-id" | "iam" => true,
            _ => {
                return Err(ParseError::AccountInvalid {
                    index,
                    reason: InvalidAccountReason::AuthMethodUnsupported,
                });
            }
        };
        if explicit_idc != idc {
            return Err(ParseError::AccountInvalid {
                index,
                reason: InvalidAccountReason::AuthMethodMismatch,
            });
        }
    }
    let refresh_token = required_text(index, credentials.refresh_token, "refreshToken")?;
    let access_token = credentials.access_token.unwrap_or_default();
    let region = credentials.region.unwrap_or_else(|| "us-east-1".to_owned());
    if idc && access_token.trim().is_empty() {
        return Err(ParseError::AccountInvalid {
            index,
            reason: InvalidAccountReason::CredentialMissing("accessToken"),
        });
    }
    if !valid_region(&region) {
        return Err(ParseError::AccountInvalid {
            index,
            reason: InvalidAccountReason::RegionInvalid,
        });
    }

    let has_expiry = credentials.expires_at.is_some();
    let expires_at = match credentials.expires_at {
        Some(value) => unix_seconds(&value),
        None if idc => None,
        None => Some(0),
    }
    .ok_or(ParseError::AccountInvalid {
        index,
        reason: if has_expiry {
            InvalidAccountReason::ExpiryOutOfRange
        } else {
            InvalidAccountReason::ExpiryMissing
        },
    })?;
    let auth_region = credentials.auth_region.unwrap_or_else(|| region.clone());
    let api_region = credentials.api_region.unwrap_or_else(|| region.clone());
    let credential_profile_arn = credentials.profile_arn;
    if !valid_region(&auth_region) || !valid_region(&api_region) {
        return Err(ParseError::AccountInvalid {
            index,
            reason: InvalidAccountReason::RegionInvalid,
        });
    }
    let credentials = ValidatedCredentials {
        client_id,
        client_secret,
        refresh_token,
        access_token,
        region,
        auth_region,
        api_region,
    };
    let profile_arn =
        account
            .profile_arn
            .or(credential_profile_arn)
            .and_then(|value| match value {
                serde_json::Value::String(value) if !value.trim().is_empty() => {
                    Some(value.trim().to_owned())
                }
                _ => None,
            });

    Ok(validated_account(credentials, expires_at, profile_arn))
}

fn validated_account(
    credentials: ValidatedCredentials,
    expires_at: i64,
    profile_arn: Option<String>,
) -> ValidatedAccount {
    ValidatedAccount {
        credentials,
        expires_at,
        profile_arn,
    }
}

fn required_text(
    index: usize,
    value: Option<String>,
    field: &'static str,
) -> Result<String, ParseError> {
    let value = value.ok_or(ParseError::AccountInvalid {
        index,
        reason: InvalidAccountReason::CredentialMissing(field),
    })?;
    if value.trim().is_empty() {
        return Err(ParseError::AccountInvalid {
            index,
            reason: InvalidAccountReason::CredentialEmpty(field),
        });
    }
    Ok(if value.trim().len() == value.len() {
        value
    } else {
        value.trim().to_owned()
    })
}

/// Accept the raw export's millisecond epoch as well as the normalized
/// server-side Unix-seconds and RFC3339 forms.
fn unix_seconds(expires_at: &serde_json::Value) -> Option<i64> {
    match expires_at {
        serde_json::Value::Number(number) => {
            let value = number
                .as_i64()
                .map(i128::from)
                .or_else(|| number.as_u64().map(i128::from))?;
            if value < 0 {
                return None;
            }
            let seconds = if value >= i128::from(MIN_MILLISECOND_EPOCH) {
                value / 1_000
            } else {
                value
            };
            i64::try_from(seconds).ok()
        }
        serde_json::Value::String(value) => chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|timestamp| timestamp.timestamp())
            .filter(|timestamp| *timestamp > 0),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Export {
    accounts: Option<Vec<Account>>,
}

#[derive(Deserialize)]
struct Account {
    credentials: Option<CredentialsInput>,
    #[serde(rename = "profileArn")]
    profile_arn: Option<serde_json::Value>,
}

#[derive(Debug)]
struct ValidatedAccount {
    credentials: ValidatedCredentials,
    expires_at: i64,
    profile_arn: Option<String>,
}

#[derive(Debug)]
struct ValidatedCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    access_token: String,
    region: String,
    auth_region: String,
    api_region: String,
}

#[derive(Deserialize)]
struct CredentialsInput {
    #[serde(rename = "clientId")]
    client_id: Option<String>,
    #[serde(rename = "clientSecret")]
    client_secret: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
    region: Option<String>,
    #[serde(rename = "authRegion")]
    auth_region: Option<String>,
    #[serde(rename = "apiRegion")]
    api_region: Option<String>,
    #[serde(rename = "authMethod")]
    auth_method: Option<String>,
    #[serde(rename = "profileArn")]
    profile_arn: Option<serde_json::Value>,
}
