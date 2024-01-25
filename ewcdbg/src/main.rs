use wayrs_client::{global::GlobalsExt, Connection, IoMode};

wayrs_client::generate!("../src/protocol/ewc-debug-v1.xml");

fn main() {
    let (mut conn, globals) = Connection::<()>::connect_and_collect_globals().unwrap();

    let _debugger: EwcDebugV1 = globals
        .bind_with_cb(&mut conn, 1, |ctx| {
            let ewc_debug_v1::Event::Message(msg) = ctx.event;
            println!("{}", msg.to_str().unwrap());
        })
        .expect("unsupported compositor");

    loop {
        conn.flush(IoMode::Blocking).unwrap();
        conn.recv_events(IoMode::Blocking).unwrap();
        conn.dispatch_events(&mut ());
    }
}
