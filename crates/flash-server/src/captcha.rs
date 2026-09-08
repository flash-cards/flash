//! The captcha gate on the public email-sending forms (password reset
//! here; signup in the hosted product). One trait, implemented by the
//! extension; no verifier means no captcha at all (dev / self-host).

pub trait CaptchaVerifier: Send + Sync {
    /// True iff the widget token is valid. Transport errors are Err —
    /// callers fail closed (retrying is cheap; outages are rare).
    fn verify(&self, token: &str, remote_ip: Option<&str>) -> Result<bool, String>;
}

/// Gate for the email-sending public forms. No verifier => Ok.
/// Missing/failed token or transport error => Err with user-facing copy
/// (fail closed; retrying is cheap).
pub async fn require_captcha(
    state: &crate::state::AppState,
    token: &str,
    headers: &axum::http::HeaderMap,
) -> Result<(), &'static str> {
    let Some(verifier) = state.ext.captcha_verifier(state) else {
        return Ok(());
    };
    const MSG: &str = "Please complete the verification check and try again.";
    if token.is_empty() {
        return Err(MSG);
    }
    let token = token.to_string();
    // Only a header the operator declared trustworthy is worth forwarding.
    let remote_ip = state
        .config
        .client_ip_header
        .as_deref()
        .and_then(|name| headers.get(name))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string());
    let outcome =
        tokio::task::spawn_blocking(move || verifier.verify(&token, remote_ip.as_deref())).await;
    match outcome {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(MSG),
        Ok(Err(e)) => {
            tracing::error!("captcha verify: {e}");
            Err(MSG)
        }
        Err(e) => {
            tracing::error!("captcha join: {e}");
            Err(MSG)
        }
    }
}
