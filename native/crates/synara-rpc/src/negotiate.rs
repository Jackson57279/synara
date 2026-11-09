//! `GET /ws/negotiate` over plain HTTP (the `transport.http-negotiate` path).
//! Negotiate is unauthenticated; only the `/ws` upgrade checks credentials.

use synara_protocol::negotiate::{ClientOffer, CompatibilityError, NegotiateResult};
use url::Url;

use crate::error::ConnectError;

pub async fn negotiate(
    http: &reqwest::Client,
    base: &Url,
    offer: &ClientOffer,
) -> Result<NegotiateResult, ConnectError> {
    let response = http
        .get(offer.negotiate_url(base))
        .send()
        .await
        .map_err(|e| ConnectError::Request(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|e| ConnectError::Request(e.to_string()))?;
    parse_negotiate_response(status, &body)
}

pub fn parse_negotiate_response(status: u16, body: &str) -> Result<NegotiateResult, ConnectError> {
    match status {
        200 => serde_json::from_str(body).map_err(|e| ConnectError::Http {
            status,
            body: format!("undecodable negotiate result ({e}): {body}"),
        }),
        426 => match serde_json::from_str::<CompatibilityError>(body) {
            Ok(error) => Err(ConnectError::Compatibility(error)),
            Err(_) => Err(ConnectError::Http {
                status,
                body: body.to_owned(),
            }),
        },
        _ => Err(ConnectError::Http {
            status,
            body: body.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_and_incompatible() {
        let ok = parse_negotiate_response(
            200,
            r#"{"protocolEpoch":1,"negotiatedRevision":3,"serverBuild":"b","serverInstanceId":"i","capabilities":[]}"#,
        )
        .unwrap();
        assert_eq!(ok.server_instance_id, "i");

        let err = parse_negotiate_response(
            426,
            r#"{"_tag":"WsCompatibilityError","message":"m","code":"WS_PROTOCOL_INCOMPATIBLE","action":"update-client","serverBuild":"b","protocolEpoch":2,"minRevision":1,"maxRevision":1}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConnectError::Compatibility(_)));

        assert!(matches!(
            parse_negotiate_response(403, "Forbidden"),
            Err(ConnectError::Http { status: 403, .. })
        ));
    }
}
