use std::time::Duration;

use wayrs_client::{global::GlobalsExt, Connection, IoMode};

wayrs_client::generate!("../protocol/ewc-debug.xml");
use ewc_debug_v1::Interest;

const INTERESTS: &[(&str, &str, ewc_debug_v1::Interest)] = &[
    ("frame", "frame timings", Interest::FrameStat),
    ("message", "arbitrary debug messages", Interest::Messages),
];

fn usage() -> ! {
    println!("Usage: ewcdbg interest1 [interest2 [...]]");
    println!("Possible interests");
    for (interest, desc, _) in INTERESTS {
        println!("  {interest} - {desc}");
    }
    std::process::exit(1);
}

fn main() {
    let mut interest = Interest::None;
    for arg in std::env::args().skip(1) {
        if let Some((_, _, i)) = INTERESTS.iter().find(|i| i.0 == arg) {
            interest |= *i;
        } else {
            usage();
        }
    }
    if interest == Interest::None {
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
            Event::Massage(msg) => {
                println!("msg: {}", msg.to_str().unwrap());
            }
        }
    });

    loop {
        conn.flush(IoMode::Blocking).unwrap();
        conn.recv_events(IoMode::Blocking).unwrap();
        conn.dispatch_events(&mut ());
    }
}
