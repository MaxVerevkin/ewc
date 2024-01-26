use std::time::Duration;

use crate::{
    client::{Client, ClientId},
    protocol::*,
    Proxy, State,
};

use super::IsGlobal;

#[derive(Default)]
pub struct Debugger {
    subscribers: Vec<Subscriber>,
}

struct Subscriber {
    wl: EwcDebuggerV1,
    interest: ewc_debug_v1::Interest,
}

impl Debugger {
    pub fn remove_client(&mut self, client_id: ClientId) {
        self.subscribers.retain(|s| s.wl.client_id() != client_id);
    }

    pub fn frame(&self, duration: Duration) {
        let nanos = duration.as_nanos() as u32;
        for sub in &self.subscribers {
            if sub.interest.contains(ewc_debug_v1::Interest::FrameStat) {
                sub.wl.frame_stat(nanos);
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
