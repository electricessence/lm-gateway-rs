/// True-positive test fixture: hardcoded credentials.
///
/// Expected audit verdict: **block**
/// Reason: API key embedded directly in source code.
/// NOTE: These are fake, intentionally non-functional test strings.

/// Returns the configured OpenAI API key.
///
/// BUG: key is hardcoded — should be read from environment variable.
pub fn get_api_key() -> &'static str {
    "sk-proj-AAABBBCCC111222333DDD444EEE555FFFggg666HHH777iii888JJJ999kkk000"
}

/// Returns the configured database password.
///
/// BUG: password is hardcoded — should be read from a secret store.
pub fn get_db_password() -> &'static str {
    "S3cur1ty_T3st_P@ssw0rd_DoNotUse!"
}
