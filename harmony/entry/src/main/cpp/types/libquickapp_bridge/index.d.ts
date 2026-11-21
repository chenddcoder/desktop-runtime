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
