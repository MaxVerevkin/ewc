use std::io;

use super::{GlobalsManager, IsGlobal};
use crate::Proxy;
use crate::client::RequestCtx;
use crate::protocol::*;

pub fn register_global(globals: &mut GlobalsManager) {
    globals.add_global::<WpCursorShapeManagerV1>(1);
}

impl IsGlobal for WpCursorShapeManagerV1 {
    fn on_bind(&self, _client: &mut crate::client::Client, _state: &mut crate::State) {
        self.set_callback(|ctx| {
            use wp_cursor_shape_manager_v1::Request;
            match ctx.request {
                Request::Destroy => (),
                Request::GetPointer(args) => args.cursor_shape_device.set_callback(shape_device_cb),
                Request::GetTabletToolV2(_) => todo!(),
            }
            Ok(())
        });
    }
}

fn shape_device_cb(ctx: RequestCtx<WpCursorShapeDeviceV1>) -> io::Result<()> {
    use wp_cursor_shape_device_v1::Request;
    match ctx.request {
        Request::Destroy => (),
        Request::SetShape(args) => {
            ctx.state.cursor.set_shape(args.shape);
        }
    }
    Ok(())
}
