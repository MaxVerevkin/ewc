use std::{ffi::CString, time::Duration};

use crate::{
    Proxy, State,
    client::{Client, ClientId},
    protocol::*,
};

use super::IsGlobal;

#[derive(Default)]
pub struct Debugger {
    subscribers: Vec<Subscriber>,
    accum_interest: ewc_debug_v1::Interest,
}

struct Subscriber {
    wl: EwcDebuggerV1,
    interest: ewc_debug_v1::Interest,
}

impl Debugger {
    pub fn remove_client(&mut self, client_id: ClientId) {
        self.subscribers.retain(|s| s.wl.client_id() != client_id);
        self.accum_interest = self
            .subscribers
            .iter()
            .fold(ewc_debug_v1::Interest::None, |acc, s| acc | s.interest);
    }

    pub fn accum_interest(&self) -> ewc_debug_v1::Interest {
        self.accum_interest
    }

    pub fn frame(&self, duration: Duration) {
        let nanos = duration.as_nanos() as u32;
        for sub in &self.subscribers {
            if sub.interest.contains(ewc_debug_v1::Interest::FrameStat) {
                sub.wl.frame_stat(nanos);
            }
        }
    }

    pub fn message(&self, msg: &str) {
        let cstr = CString::new(msg).expect("debug message has null bytes");
        for sub in &self.subscribers {
            if sub.interest.contains(ewc_debug_v1::Interest::Messages) {
                sub.wl.massage(cstr.clone());
            }
        }
    }
}

impl IsGlobal for EwcDebugV1 {
    fn on_bind(&self, _client: &mut Client, _state: &mut State) {
        self.set_callback(|ctx| {
            use ewc_debug_v1::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::GetDebugger(args) => {
                    args.id.set_callback(|ctx| match ctx.request {});
                    ctx.state.debugger.accum_interest |= args.interest;
                    ctx.state.debugger.subscribers.push(Subscriber {
                        wl: args.id,
                        interest: args.interest,
                    });
                }
            }
            Ok(())
        });
    }
}
