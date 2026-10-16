//! Reports ABI 99 so the host can show a failed load and ignore the file.

use seasound_plugin_api::{PluginInfo, KIND_DECODER};

const ID: &str = "badabi\0";
const NAME: &str = "Bad ABI\0";
const VERSION: &str = "0.0.0\0";

#[no_mangle]
pub extern "C" fn seasound_plugin_info() -> *const PluginInfo {
    static INFO: PluginInfo = PluginInfo {
        abi: 99,
        kind: KIND_DECODER,
        id: ID.as_ptr().cast(),
        name: NAME.as_ptr().cast(),
        version: VERSION.as_ptr().cast(),
    };
    &INFO
}
