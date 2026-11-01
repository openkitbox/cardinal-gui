//! Pure offline validator for pasted Kiro credential documents.
//!
//! Accepted roots: a JSON array of accounts (each element a flat `kiro.rs`
//! credential object or `{profileArn, credentials: {...}}`), the IDE export
//! `{accounts: [{profileArn, credentials: {...}}]}`, or one flat credential
//! object. The gateway owns the credential format
//! (`gateway::kiro::parse_import_credentials`); this module only validates for
//! the preview and keeps every account's original JSON element, which is what
//! the form sends. The rules mirror the server's: a missing `expiresAt` means
//! 0 (refresh before first use), a missing `region` is us-east-1, and
//! `authRegion`/`apiRegion` default to `region`. A missing, non-string, or
//! blank account-level `profileArn` counts as absent.

use std::fmt;

use gateway::kiro::valid_region;
use serde_json::Map;
use serde_json::Value;

/// Numeric `expiresAt` at or above this is milliseconds (the gateway's cut).
const MIN_MILLISECOND_EPOCH: i64 = 100_000_000_000;

/// The region the gateway assumes when an account omits `region`.
const DEFAULT_REGION: &str = "us-east-1";

/// A validated document. The private fields keep the credential elements out
/// of callers that only need preview UI; they leave only through
/// [`ParsedKiroExport::into_import_items`].
pub struct ParsedKiroExport {
    accounts: Vec<ValidatedAccount>,
}

/// Secret-free information about one account, for the preview.
#[derive(Clone, Copy, Debug)]
pub struct AccountSummary<'a> {
    pub region: &'a str,
    pub api_region: &'a str,
    pub expires_at: i64,
    pub has_profile_arn: bool,
}

/// The accounts of one document do not share an API region; a Kiro provider
/// row serves exactly one runtime region, so such a document must be split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixedApiRegions;

impl fmt::Display for MixedApiRegions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("账号的 API region 不一致，请分开导入")
    }
}

/// One account ready for `POST /api/providers/{id}/kiro-import`.
pub struct ImportItem {
    /// The account's original JSON element wrapped in a one-element array.
    /// Only the gateway's array branch (account 0) reads both element shapes:
    /// sent bare, a nested `{profileArn, credentials}` element would be read
    /// as a flat credential and rejected.
    pub credentials: Value,
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
    NotAnObject,
    CredentialsMissing,
    FieldNotString(&'static str),
    CredentialMissing(&'static str),
    CredentialEmpty(&'static str),
    ClientBundleIncomplete,
    AuthMethodUnsupported,
    AuthMethodMismatch,
    ExpiryOutOfRange,
    RegionInvalid,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDocument => {
                formatter.write_str("Kiro JSON must be an object or an array of objects")
            }
            Self::AccountsMissing => formatter.write_str("Kiro export has no accounts array"),
            Self::AccountsEmpty => formatter.write_str("Kiro JSON contains no accounts"),
            Self::AccountInvalid { index, reason } => {
                write!(formatter, "Kiro account {} is invalid: {reason}", index + 1)
            }
        }
    }
}

impl fmt::Display for InvalidAccountReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => formatter.write_str("account must be a JSON object"),
            Self::CredentialsMissing => formatter.write_str("credentials object is missing"),
            Self::FieldNotString(field) => write!(formatter, "{field} must be a string"),
            Self::CredentialMissing(field) => write!(formatter, "{field} is missing"),
            Self::CredentialEmpty(field) => write!(formatter, "{field} is empty"),
            Self::ClientBundleIncomplete => {
                formatter.write_str("clientId and clientSecret must be provided together")
            }
            Self::AuthMethodUnsupported => formatter.write_str("authMethod is not supported"),
            Self::AuthMethodMismatch => {
                formatter.write_str("authMethod does not match the supplied credential fields")
            }
            Self::ExpiryOutOfRange => formatter.write_str("expiresAt is invalid"),
            Self::RegionInvalid => formatter.write_str("region is not a valid AWS region"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Where an element came from: elements of the IDE export's `accounts` array
/// must nest their credential; array elements and a bare root may be flat.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ElementSource {
    AccountsArray,
    Loose,
}

/// Parse and validate a document without retaining its raw JSON text.
pub fn parse(input: &str) -> Result<ParsedKiroExport, ParseError> {
    let document: Value = serde_json::from_str(input).map_err(|_| ParseError::InvalidDocument)?;
    let (elements, source) = match document {
        Value::Array(elements) => (elements, ElementSource::Loose),
        Value::Object(mut root) => match root.remove("accounts") {
            Some(Value::Array(accounts)) => (accounts, ElementSource::AccountsArray),
            Some(Value::Null) => return Err(ParseError::AccountsMissing),
            Some(_) => return Err(ParseError::InvalidDocument),
            None => (vec![Value::Object(root)], ElementSource::Loose),
        },
        _ => return Err(ParseError::InvalidDocument),
    };
    if elements.is_empty() {
        return Err(ParseError::AccountsEmpty);
    }
    let accounts: Vec<ValidatedAccount> = elements
        .into_iter()
        .enumerate()
        .map(|(index, element)| validate_account(index, element, source))
        .collect::<Result<_, _>>()?;
    Ok(ParsedKiroExport { accounts })
}

/// Save-time parse boundary. Keeping this named separately makes it explicit
/// that the current textarea contents, rather than a stale UI snapshot, are
/// the source of the credentials sent to the gateway.
pub fn parse_for_save(input: &str) -> Result<ParsedKiroExport, ParseError> {
    parse(input)
}

/// Trim and check the provider name and the label (or label prefix).
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

/// `count` fresh labels `{prefix}-NNN`, numbered after the largest N among
/// `existing` labels of exactly that form (digits only after `{prefix}-`).
/// Numbers are zero-padded to three digits; wider numbers keep their width.
#[must_use]
pub fn next_labels<'a>(
    prefix: &str,
    existing: impl Iterator<Item = &'a str>,
    count: usize,
) -> Vec<String> {
    let start = first_free_number(prefix, existing);
    (0..count as u64)
        .map(|offset| numbered_label(prefix, start.saturating_add(offset)))
        .collect()
}

/// The first and last label [`next_labels`] would produce, without building
/// the whole list (render-time preview). `None` when `count` is 0.
#[must_use]
pub fn label_span<'a>(
    prefix: &str,
    existing: impl Iterator<Item = &'a str>,
    count: usize,
) -> Option<(String, String)> {
    let last_offset = (count as u64).checked_sub(1)?;
    let start = first_free_number(prefix, existing);
    Some((
        numbered_label(prefix, start),
        numbered_label(prefix, start.saturating_add(last_offset)),
    ))
}

fn first_free_number<'a>(prefix: &str, existing: impl Iterator<Item = &'a str>) -> u64 {
    existing
        .filter_map(|label| label.strip_prefix(prefix)?.strip_prefix('-'))
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .filter_map(|digits| digits.parse::<u64>().ok())
        .max()
        .map_or(1, |max| max.saturating_add(1))
}

fn numbered_label(prefix: &str, number: u64) -> String {
    format!("{prefix}-{number:03}")
}

impl ParsedKiroExport {
    /// Number of accounts in the document (never 0).
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// Non-secret account metadata for the preview.
    pub fn summaries(&self) -> impl Iterator<Item = AccountSummary<'_>> + '_ {
        self.accounts.iter().map(|account| AccountSummary {
            region: &account.region,
            api_region: &account.api_region,
            expires_at: account.expires_at,
            has_profile_arn: account.has_profile_arn,
        })
    }

    /// The API region every account shares (it selects the provider's
    /// runtime endpoint); an error when the accounts disagree.
    pub fn api_region(&self) -> Result<&str, MixedApiRegions> {
        let mut regions = self
            .accounts
            .iter()
            .map(|account| account.api_region.as_str());
        let first = regions.next().ok_or(MixedApiRegions)?;
        if regions.all(|region| region == first) {
            Ok(first)
        } else {
            Err(MixedApiRegions)
        }
    }

    /// The labels to import under, one per account in document order. A
    /// single-account document uses `label` verbatim; a multi-account
    /// document treats it as a prefix and numbers after the largest existing
    /// `{prefix}-N` (see [`next_labels`]).
    pub fn import_labels<'a>(
        &self,
        label: &str,
        existing: impl Iterator<Item = &'a str>,
    ) -> Result<Vec<String>, &'static str> {
        let label = label.trim();
        if label.is_empty() {
            return Err("label 不能为空");
        }
        if self.accounts.len() == 1 {
            return Ok(vec![label.to_owned()]);
        }
        if label.contains('/') {
            return Err("label 前缀不能包含 /");
        }
        Ok(next_labels(label, existing, self.accounts.len()))
    }

    /// Hand over every account's original JSON element, in document order.
    pub fn into_import_items(self) -> Vec<ImportItem> {
        self.accounts
            .into_iter()
            .map(|account| ImportItem {
                credentials: Value::Array(vec![account.element]),
            })
            .collect()
    }
}

fn validate_account(
    index: usize,
    element: Value,
    source: ElementSource,
) -> Result<ValidatedAccount, ParseError> {
    let invalid = |reason| ParseError::AccountInvalid { index, reason };
    let checked = check_element(&element, source).map_err(invalid)?;
    Ok(ValidatedAccount {
        element,
        region: checked.region,
        api_region: checked.api_region,
        expires_at: checked.expires_at,
        has_profile_arn: checked.has_profile_arn,
    })
}

/// The secret-free facts read from one element.
struct CheckedElement {
    region: String,
    api_region: String,
    expires_at: i64,
    has_profile_arn: bool,
}

/// Validate one element by reference, the way the gateway will read it once
/// wrapped as `[element]`: a `credentials` object nests the credential, else
/// the element itself is the flat credential.
fn check_element(
    element: &Value,
    source: ElementSource,
) -> Result<CheckedElement, InvalidAccountReason> {
    let account = element
        .as_object()
        .ok_or(InvalidAccountReason::NotAnObject)?;
    let (credentials, account_profile_arn) =
        match account.get("credentials").and_then(Value::as_object) {
            Some(credentials) => (credentials, account.get("profileArn")),
            None if source == ElementSource::AccountsArray => {
                return Err(InvalidAccountReason::CredentialsMissing);
            }
            None => (account, None),
        };

    let client_id = string_field(credentials, "clientId")?.unwrap_or_default();
    let client_secret = string_field(credentials, "clientSecret")?.unwrap_or_default();
    let has_client_id = !client_id.trim().is_empty();
    let has_client_secret = !client_secret.trim().is_empty();
    if has_client_id != has_client_secret {
        return Err(InvalidAccountReason::ClientBundleIncomplete);
    }
    let idc = has_client_id && has_client_secret;
    if let Some(auth_method) = string_field(credentials, "authMethod")? {
        // The gateway's `AuthMethod` spellings, case-sensitive like its serde.
        let explicit_idc = match auth_method {
            "social" => false,
            "idc" | "IdC" | "builder-id" | "iam" => true,
            _ => return Err(InvalidAccountReason::AuthMethodUnsupported),
        };
        if explicit_idc != idc {
            return Err(InvalidAccountReason::AuthMethodMismatch);
        }
    }
    match string_field(credentials, "refreshToken")? {
        None => return Err(InvalidAccountReason::CredentialMissing("refreshToken")),
        Some(token) if token.trim().is_empty() => {
            return Err(InvalidAccountReason::CredentialEmpty("refreshToken"));
        }
        Some(_) => {}
    }
    let access_token = string_field(credentials, "accessToken")?.unwrap_or_default();
    if idc && access_token.trim().is_empty() {
        return Err(InvalidAccountReason::CredentialMissing("accessToken"));
    }
    let expires_at = match credentials.get("expiresAt") {
        None | Some(Value::Null) => 0,
        Some(value) => unix_seconds(value).ok_or(InvalidAccountReason::ExpiryOutOfRange)?,
    };
    let region = string_field(credentials, "region")?.unwrap_or(DEFAULT_REGION);
    let auth_region = string_field(credentials, "authRegion")?.unwrap_or(region);
    let api_region = string_field(credentials, "apiRegion")?.unwrap_or(region);
    if !valid_region(region) || !valid_region(auth_region) || !valid_region(api_region) {
        return Err(InvalidAccountReason::RegionInvalid);
    }
    let has_profile_arn = string_field(credentials, "profileArn")?
        .is_some_and(|value| !value.trim().is_empty())
        || account_profile_arn
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
    Ok(CheckedElement {
        region: region.to_owned(),
        api_region: api_region.to_owned(),
        expires_at,
        has_profile_arn,
    })
}

/// A string credential field, borrowed; absent and `null` are `None`, any
/// other type is rejected (the gateway rejects it too).
fn string_field<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
) -> Result<Option<&'a str>, InvalidAccountReason> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(InvalidAccountReason::FieldNotString(name)),
    }
}

/// Accept the raw export's millisecond epoch as well as the normalized
/// server-side Unix-seconds and RFC3339 forms.
fn unix_seconds(expires_at: &Value) -> Option<i64> {
    match expires_at {
        Value::Number(number) => {
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
        Value::String(value) => chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|timestamp| timestamp.timestamp())
            .filter(|timestamp| *timestamp > 0),
        _ => None,
    }
}

/// One validated account: the original element (sent as-is) plus the
/// secret-free facts the preview shows.
struct ValidatedAccount {
    element: Value,
    region: String,
    api_region: String,
    expires_at: i64,
    has_profile_arn: bool,
}
