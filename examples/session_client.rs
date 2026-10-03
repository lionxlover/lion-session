//! `session_client` — reference client for the `os.lionos.Session1`
//! service (spec 02 §4). Mirrors lion-greeter's `greeter_client`:
//! scriptable, dependency-light, and the basis for the e2e tests.
//!
//! Run: `session_client <command> [args]`
//! Commands: status, logout, restart, shutdown, suspend, hibernate,
//! lock, switch-user, register APP_ID, inhibit WHAT WHO WHY, signals.

use zbus::zvariant::OwnedFd;

#[zbus::proxy(
    interface = "os.lionos.Session1",
    default_service = "os.lionos.Session1",
    default_path = "/os/lionos/Session1"
)]
trait Session1 {
    fn logout(&self) -> zbus::Result<()>;
    fn restart(&self) -> zbus::Result<()>;
    fn shutdown(&self) -> zbus::Result<()>;
    fn suspend(&self) -> zbus::Result<()>;
    fn hibernate(&self) -> zbus::Result<()>;
    fn lock(&self) -> zbus::Result<()>;
    fn switch_user(&self) -> zbus::Result<()>;
    fn inhibit(&self, what: &str, who: &str, why: &str) -> zbus::Result<OwnedFd>;
    fn register_client(&self, app_id: &str) -> zbus::Result<()>;
    /// Documented extension: ack a QueryEndSession without disconnecting.
    fn end_session_reply(&self, app_id: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn inhibited_actions(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn safe_mode(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn blockers(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn session_ready(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn query_end_session(&self, flags: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn end_session(&self, flags: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn service_failed(&self, name: &str, reason: &str) -> zbus::Result<()>;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "status".into());
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("session_client: session bus unavailable: {e}");
            std::process::exit(1);
        }
    };
    let session = Session1Proxy::new(&conn)
        .await
        .expect("connect to os.lionos.Session1");

    let r: zbus::Result<()> = match cmd.as_str() {
        "status" => {
            let state = session
                .state()
                .await
                .unwrap_or_else(|e| format!("<error: {e}>"));
            let inhibited = session
                .inhibited_actions()
                .await
                .unwrap_or_default()
                .join(",");
            let safe = session.safe_mode().await.unwrap_or(false);
            let blockers = session.blockers().await.unwrap_or_default().join("; ");
            println!("State:            {state}");
            println!("InhibitedActions: {inhibited}");
            println!("SafeMode:         {safe}");
            println!("Blockers:         {blockers}");
            Ok(())
        }
        "logout" => session.logout().await,
        "restart" => session.restart().await,
        "shutdown" => session.shutdown().await,
        "suspend" => session.suspend().await,
        "hibernate" => session.hibernate().await,
        "lock" => session.lock().await,
        "switch-user" => session.switch_user().await,
        "register" => {
            let app_id = args.get(1).expect("usage: register APP_ID").clone();
            session.register_client(&app_id).await?;
            println!("registered as {app_id}; awaiting end-session queries (Ctrl-C to exit)");
            use futures_util::StreamExt;
            let mut queries = session
                .receive_query_end_session()
                .await
                .expect("subscribe QueryEndSession");
            while let Some(q) = queries.next().await {
                let args = q.args().expect("query args");
                println!("QueryEndSession(flags={})", args.flags());
                println!("acking in 1s…");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                session.end_session_reply(&app_id).await?;
                println!("acked");
            }
            Ok(())
        }
        "inhibit" => {
            let what = args.get(1).expect("usage: inhibit WHAT WHO WHY").clone();
            let who = args.get(2).expect("usage: inhibit WHAT WHO WHY").clone();
            let why = args.get(3).expect("usage: inhibit WHAT WHO WHY").clone();
            let fd = session.inhibit(&what, &who, &why).await?;
            println!("inhibitor held for {what}; Ctrl-C to release");
            // Holding the fd keeps the inhibitor; dropping/exiting releases.
            tokio::signal::ctrl_c().await.ok();
            drop(fd);
            Ok(())
        }
        "signals" => {
            println!("watching signals; Ctrl-C to exit");
            use futures_util::StreamExt;
            let mut ready = session.receive_session_ready().await?;
            let mut query = session.receive_query_end_session().await?;
            let mut end = session.receive_end_session().await?;
            let mut failed = session.receive_service_failed().await?;
            loop {
                tokio::select! {
                    Some(_) = ready.next() => println!("<SessionReady>"),
                    Some(q) = query.next() => {
                        let a = q.args().expect("args");
                        println!("<QueryEndSession flags={}>", a.flags());
                    }
                    Some(e) = end.next() => {
                        let a = e.args().expect("args");
                        println!("<EndSession flags={}>", a.flags());
                    }
                    Some(f) = failed.next() => {
                        let a = f.args().expect("args");
                        println!("<ServiceFailed {} {}>", a.name(), a.reason());
                    }
                    else => break,
                }
            }
            Ok(())
        }
        other => {
            eprintln!(
                "unknown command {other:?} (status|logout|restart|shutdown|suspend|hibernate|lock|switch-user|register|inhibit|signals)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("session_client: {cmd} failed: {e}");
        std::process::exit(1);
    }
    Ok(())
}
