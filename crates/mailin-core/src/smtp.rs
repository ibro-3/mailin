//! Message submission over SMTP.

use std::time::Duration;

use lettre::address::{Address, Envelope};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::account::{Security, ServerSettings};
use crate::error::{Error, Result};
use crate::model::OutgoingEnvelope;

const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Submit a raw RFC 5322 message. The envelope is supplied separately so that
/// `Bcc` recipients are honoured without the header leaking to other people.
pub(crate) async fn send(
    settings: &ServerSettings,
    password: &str,
    raw: &[u8],
    outgoing: &OutgoingEnvelope,
) -> Result<()> {
    outgoing.validate()?;

    let tls = match settings.security {
        Security::Tls => Tls::Wrapper(tls_parameters(&settings.host)?),
        Security::StartTls => Tls::Required(tls_parameters(&settings.host)?),
        Security::None => Tls::None,
    };

    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(settings.host.as_str())
        .port(settings.port)
        .credentials(Credentials::new(
            settings.username.clone(),
            password.to_owned(),
        ))
        .tls(tls)
        .timeout(Some(SEND_TIMEOUT))
        .build();

    let from: Address = outgoing
        .from
        .parse()
        .map_err(|e| Error::Smtp(format!("invalid sender address {:?}: {e}", outgoing.from)))?;
    let mut recipients =
        Vec::with_capacity(outgoing.to.len() + outgoing.cc.len() + outgoing.bcc.len());
    for address in outgoing.recipients() {
        recipients.push(
            address
                .parse::<Address>()
                .map_err(|e| Error::Smtp(format!("invalid recipient address {address:?}: {e}")))?,
        );
    }
    let envelope = Envelope::new(Some(from), recipients).map_err(|e| Error::Smtp(e.to_string()))?;

    transport
        .send_raw(&envelope, raw)
        .await
        .map_err(|e| Error::Smtp(format!("sending failed: {e}")))?;
    Ok(())
}

fn tls_parameters(host: &str) -> Result<TlsParameters> {
    TlsParameters::new(host.to_owned())
        .map_err(|e| Error::Tls(format!("could not prepare TLS for {host}: {e}")))
}

/// Test SMTP connection by connecting and authenticating.
pub(crate) async fn test_connection(
    settings: &ServerSettings,
    password: &str,
) -> Result<()> {
    let tls = match settings.security {
        Security::Tls => Tls::Wrapper(tls_parameters(&settings.host)?),
        Security::StartTls => Tls::Required(tls_parameters(&settings.host)?),
        Security::None => Tls::None,
    };

    let transport: AsyncSmtpTransport<Tokio1Executor> = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(settings.host.as_str())
        .port(settings.port)
        .credentials(Credentials::new(
            settings.username.clone(),
            password.to_owned(),
        ))
        .tls(tls)
        .timeout(Some(SEND_TIMEOUT))
        .build();

    // Test the connection by sending a NOOP command
    let _ = transport.test_connection().await
        .map_err(|e| Error::Smtp(format!("connection test failed: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_requires_a_sender_and_a_recipient() {
        let empty = OutgoingEnvelope::default();
        assert!(empty.validate().is_err());

        let mut outgoing = OutgoingEnvelope {
            from: "ada@example.com".into(),
            ..Default::default()
        };
        assert!(outgoing.validate().is_err());

        outgoing.to.push("grace@example.com".into());
        outgoing.validate().expect("valid envelope");

        outgoing.to.push("   ".into());
        assert!(outgoing.validate().is_err(), "empty addresses are rejected");
    }

    #[test]
    fn recipients_include_to_cc_and_bcc() {
        let outgoing = OutgoingEnvelope {
            from: "ada@example.com".into(),
            to: vec!["grace@example.com".into()],
            cc: vec!["carol@example.com".into()],
            bcc: vec!["dave@example.com".into()],
        };
        assert_eq!(
            outgoing.recipients(),
            vec!["grace@example.com", "carol@example.com", "dave@example.com"]
        );
    }
}
