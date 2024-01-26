use std::time::Duration;

use wayrs_client::{global::GlobalsExt, Connection, IoMode};

wayrs_client::generate!("../src/protocol/ewc-debug-v1.xml");

fn usage() -> ! {
    println!("Usage: ewcdbg interest1 [interest2 [...]]");
    println!("Possible interests");
    println!("  frame - display frame timings");
    std::process::exit(1);
}

fn main() {
    let mut interest = ewc_debug_v1::Interest::None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "frame" => interest |= ewc_debug_v1::Interest::FrameStat,
            _other => usage(),
        }
    }
    if interest == ewc_debug_v1::Interest::None {
        usage();
    }

    let (mut conn, globals) = Connection::<()>::connect_and_collect_globals().unwrap();

    let debug: EwcDebugV1 = globals.bind(&mut conn, 1).expect("unsupported compositor");
    debug.get_debugger_with_cb(&mut conn, interest, |ctx| {
        use ewc_debugger_v1::Event;
        match ctx.event {
            Event::FrameStat(nanos) => {
                let dur = Duration::from_nanos(nanos as u64);
                println!("frame composed in {dur:?}");
            }
        }
    });

    loop {
        conn.flush(IoMode::Blocking).unwrap();
        conn.recv_events(IoMode::Blocking).unwrap();
        conn.dispatch_events(&mut ());
    }
}
