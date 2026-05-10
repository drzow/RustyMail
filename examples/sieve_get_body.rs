// One-shot helper: print the body of a named script. Used to peek at
// the existing active filter before we decide how to wire ours in.

use std::time::Duration;

use rustymail::managesieve::{
    connect_starttls, resolve_for_account,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let name = std::env::args().nth(1).expect("usage: sieve_get_body <script>");
    let creds = resolve_for_account("config/accounts.json", None)
        .expect("resolve creds");
    let mut client = connect_starttls(&creds.host, creds.port, Duration::from_secs(15))
        .await
        .expect("connect");
    client.authenticate_plain(&creds.username, &creds.password)
        .await.expect("auth");
    let body = client.get_script(&name).await.expect("get_script");
    println!("--- {name} ---");
    println!("{body}");
    println!("--- end ---");
    let _ = client.logout().await;
}
