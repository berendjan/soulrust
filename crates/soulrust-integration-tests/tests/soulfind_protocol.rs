//! Protocol-level tests against a real Soulfind server: our soulseek-proto
//! encoding/decoding against an independent implementation of the protocol.

use soulseek_proto::server::{
    FileSearchRequest, GetPeerAddressRequest, LoginResponse, ServerMessage, ServerRequest,
    SetWaitPort,
};
use soulrust_integration_tests::{start_soulfind, unique_username, ServerConnection};

#[test]
fn soulfind_protocol_suite() {
    // One container for the whole suite: containers are expensive, and the
    // scenarios are independent connections anyway.
    let (_container, port) = start_soulfind();

    login_succeeds(port);
    wrong_password_after_registration_fails(port);
    set_wait_port_is_reflected_in_get_peer_address(port);
    file_search_leaves_connection_healthy(port);
}

fn login_succeeds(port: u16) {
    let mut connection = ServerConnection::connect(port).unwrap();
    let response = connection.login(&unique_username("login"), "secret").unwrap();
    match response {
        LoginResponse::Success { greeting, .. } => {
            // Soulfind sends a MOTD; contents are server-configured.
            let _ = greeting;
        }
        LoginResponse::Failure { reason, .. } => panic!("login rejected: {reason}"),
    }
}

fn wrong_password_after_registration_fails(port: u16) {
    let username = unique_username("pwcheck");

    let mut first = ServerConnection::connect(port).unwrap();
    let response = first.login(&username, "right-password").unwrap();
    assert!(
        matches!(response, LoginResponse::Success { .. }),
        "first login registers the account"
    );
    drop(first);

    let mut second = ServerConnection::connect(port).unwrap();
    let response = second.login(&username, "wrong-password").unwrap();
    match response {
        LoginResponse::Failure { reason, .. } => {
            assert!(
                reason.to_uppercase().contains("PASS"),
                "expected a password rejection, got: {reason}"
            );
        }
        LoginResponse::Success { .. } => panic!("wrong password was accepted"),
    }
}

fn set_wait_port_is_reflected_in_get_peer_address(port: u16) {
    let username = unique_username("waitport");
    let mut connection = ServerConnection::connect(port).unwrap();
    let response = connection.login(&username, "secret").unwrap();
    assert!(matches!(response, LoginResponse::Success { .. }));

    let wait_port = SetWaitPort { port: 2345, obfuscation_type: 0, obfuscated_port: 0 };
    connection.send_frame(&wait_port.to_frame()).unwrap();

    let request = GetPeerAddressRequest { username: username.clone() };
    connection.send_frame(&request.to_frame()).unwrap();

    let address = connection
        .read_until(|message| match message {
            ServerMessage::GetPeerAddress(response) if response.username == username => {
                Some(response)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(address.port, 2345, "server must reflect the advertised port");
}

fn file_search_leaves_connection_healthy(port: u16) {
    let username = unique_username("search");
    let mut connection = ServerConnection::connect(port).unwrap();
    assert!(matches!(
        connection.login(&username, "secret").unwrap(),
        LoginResponse::Success { .. }
    ));

    let search = FileSearchRequest { token: 4242, query: "test query".into() };
    connection.send_frame(&search.to_frame()).unwrap();

    // The server may or may not relay our own search back; what must hold is
    // that the connection still answers requests afterwards.
    let request = GetPeerAddressRequest { username: username.clone() };
    connection.send_frame(&request.to_frame()).unwrap();
    let address = connection
        .read_until(|message| match message {
            ServerMessage::GetPeerAddress(response) if response.username == username => {
                Some(response)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(address.username, username);
}
