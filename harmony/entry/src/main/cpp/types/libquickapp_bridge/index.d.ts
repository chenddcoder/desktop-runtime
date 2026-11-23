/**
 * 与 harmony-bridge（Rust / napi-ohos）的绑定声明。
 *
 * ⚠️ 这里的导出名必须与 src/lib.rs 里 `#[napi] pub fn` 的名字**逐字一致**，
 *    否则编译期能过、运行期拿到 undefined（典型症状：调用报
 *    "is not a function"，而 hvigor 一声不响）。
 */
export const ping: (argsJson: string) => string;
export const bridge_info: () => string;
export const base64_roundtrip: (input: string) => string;
export const bridge_poll: () => string;
export const webdav_set_root: (path: string) => string;
export const webdav_start: (argsJson: string) => string;
export const webdav_stop: () => string;
export const webdav_info: () => string;
export const es_read_file_base64: (path: string) => string;
export const es_list_files: () => string;
export const dlna_set_root: (path: string) => string;
export const dlna_set_local_ip: (ip: string) => string;
export const dlna_start: (argsJson: string) => string;
export const dlna_stop: () => string;
export const dlna_status: () => string;
export const dlna_report_position: (argsJson: string) => string;
export const dlna_send_remote_event: (argsJson: string) => string;
export const dlna_debug_log: (argsJson: string) => string;
export const dlna_get_name: () => string;
export const dlna_set_name: (argsJson: string) => string;
