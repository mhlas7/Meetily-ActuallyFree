# Windows Event-Loop Target Ownership

This directory vendors the published `tauri-runtime-wry` 2.11.4 crate, not an
upstream development branch.

## Provenance

- Registry archive: `tauri-runtime-wry-2.11.4.crate`
- SHA-256: `4e6fac707727b7a2f48e4ded90976324267371073edbb415ffb73bb0458d203f`
- Upstream commit: `ca90b46b2e2cbbc981dae1b809f4af4343fe0558`
- Upstream directory: `crates/tauri-runtime-wry`
- `LICENSE_APACHE-2.0`, `LICENSE_MIT`, and `.cargo_vcs_info.json` are unchanged.

Related upstream reports:

- https://github.com/tauri-apps/tauri/issues/15408
- https://github.com/tauri-apps/tauri/issues/15793
- https://github.com/tauri-apps/tauri/issues/15785
- https://github.com/tauri-apps/tauri/pull/15411
- https://github.com/tauri-apps/tao/pull/1290

The local patch is Windows-only. It must not adopt the cross-platform raw display
handle lifetime extension in PR 15411 or change Tao's internal `Rc` to `Arc`.

## Ownership Contract

`Wry` owns one strong `Arc<WindowsWindowTarget<T>>`. Its payload owns the Tao
target cloned at initialization, on the event-loop creation thread. `Wry` remains
neither Send nor Sync. The target field precedes both context and event_loop, so
dispatch expires before either context or native event-loop teardown can reenter.

Every cloned Windows context contains only a private `std::sync::Weak` to that
payload. Context/dispatcher/runtime-handle cloning and destruction on workers
therefore change atomic weak counts, never Tao's inner non-atomic Rc counts.
Final weak deallocation on a worker cannot run the already-destroyed payload's
destructor. The existing unsafe context Send/Sync implementations are retained;
no new unsafe implementations or raw access were introduced.

The sole production upgrade is in `send_user_message`, after comparing the
current Rust ThreadId to the event-loop creation ThreadId. The upgraded Arc is
local to synchronous dispatch and cannot escape through a public field/accessor.
The worker branch only reads atomic liveness metadata and sends a proxy message.
The metadata check is not a shutdown barrier: an in-flight send can race loop
closure and remains subject to the existing proxy/channel error behavior.

Windows runtime-handle monitor queries now dispatch to the event-loop thread and
return owned `Monitor` values. Closed/disconnected replies map to None or an empty
list because the 2.11.3 runtime trait has no error return for these methods. A
discarded new event-loop monitor receiver does not panic the event loop. Wry's
own monitor and cursor queries borrow its event_loop directly.

`DisplayHandle::windows()` is a safe, pointer-free constructor from the locked
raw-window-handle 0.6.2 dependency. It remains valid after Wry is destroyed and
does not require an upgrade, a raw handle message, or a fabricated lifetime. It
does not provide or extend the lifetime of an HWND.

## Run And Shutdown Semantics

Windows `run` calls the existing `run_return`, then exits the process with its
returned code. It still does not return normally. This deliberately runs Wry's
field destructors before process exit; upstream's consuming Tao run could exit
without dropping the remaining context. More importantly, callback/setup panic
now unwinds an intact Wry instead of destroying a moved-out native event_loop
before the target owner. Panics are not swallowed.

The same field order handles never-run destruction and run_return. run_iteration
does not revoke the owner: the runtime remains usable between iterations. No
target is leaked, and no destructor is queued onto a possibly stopped loop.

## Scope And Limitations

- Non-Windows ownership, monitor calls, display handles, and run behavior remain
  unchanged. This patch does not fix X11/Wayland display connection lifetimes.
- Windows runtime `tracing` is rejected at compile time. ActiveTraceSpanStore
  still contains another Rc cloned through the Send context. Enabling tracing
  requires a separate ownership fix, not removal of this guard. Application use
  of the tracing crate is distinct from tauri/tauri-runtime-wry's tracing feature.
- A worker monitor query now requires a pumping event loop. Never join a worker
  waiting for such a query from the UI thread, or call it before starting the
  loop and synchronously wait on that worker. Main-thread queries remain inline.
- Routed tasks/getters fail closed after owner destruction. Raw public event
  proxies and the preexisting direct request_exit/send_event APIs retain their
  upstream lifecycle behavior; this is not a redesign of every shutdown API.
- Existing WindowsStore/WebContext sharing, public low-level context surfaces,
  and the borrowed window-handle API are outside this target-ownership fix. This
  is not a soundness certification of the entire upstream runtime.
- WindowDispatch monitor getters already marshal through WindowMessage. Their
  Windows Tao MonitorHandle payload is only an isize (tao 0.35.2,
  src/platform_impl/windows/monitor.rs:89-90), not an EventLoopWindowTarget or Rc.
  Their existing sender unwrap behavior is unchanged; cancelled-receiver testing
  applies to the newly added event-loop monitor messages, not all upstream getters.
- The application source, audio pipeline, minibar polling, and Tao are unmodified.
  A full CUDA/custom-protocol application build passed. The reporting user later
  tried that patched build and said it worked fine so far, then authorized the
  v0.2.14 release. Trial duration is unconfirmed; no completed multi-hour recording
  soak is claimed. Long-duration field validation remains outstanding.

## Regression Tests

`src/windows_tests.rs` is explicitly registered as a Windows-only unit-test
module from lib.rs, despite the published manifest's `autotests = false`.
Each native scenario runs in its own subprocess (Tao supports one event loop per
process), with a 90-second parent timeout. No installed application is launched.

The real target Arc payload contains a test-only !Send/!Sync DropWitness, after
the Tao target. It asserts owner-thread, exactly-once destruction and observes
that the weak target has expired while the native message target still exists.
Worker-held real WryHandle/Context/DetachedWindow clones survive runtime teardown.
This tests the actual runtime ownership, not just an Arc/Weak stand-in.

Scenarios cover never-run queued tasks; repeated run_iteration; four concurrent
clone/drop workers with native hidden-window teardown; all three worker monitor
queries observed on the UI thread; worker WindowDispatch monitor getters; nested
inline dispatch (including panic with an upgraded owner in scope); dropped new
monitor receivers; run_return and consuming run setup-panic unwinding; normal run
process exit; 500 real hidden-WebView2 IPC messages; and a two-WebView stress case
with 50,000 IPC messages and five million background webview clones/drops.
The pages send messages in asynchronous batches through an in-process custom
protocol, never the network. Incognito profiles live under the isolated test
target dir. These are synthetic windows, not recordings of user audio.
Three compile-fail doctests cover target privacy and Wry's !Send/!Sync properties.

These are short deterministic lifecycle/traffic tests, not a multi-hour race
proof or an end-to-end Meetily recording test. They require a Windows desktop
and installed WebView2. Expected panic-scenario diagnostics appear on stderr.
They do not exercise Tauri AppManager's full protocol/responder integration,
multi-hour minibar churn, audio finalization, or non-Windows native libraries.

Run from the workspace root using the local MSVC-only wrapper (no CUDA/app build
helper). Build output is isolated in this vendor directory's ignored `target/`:

```text
vendor\tauri-runtime-wry\check-windows.cmd check --offline --locked -p tauri-runtime-wry
vendor\tauri-runtime-wry\check-windows.cmd test --offline --locked -p tauri-runtime-wry --lib windows_tests::windows_target_lifecycle -- --nocapture --test-threads=1
vendor\tauri-runtime-wry\check-windows.cmd test --offline --locked -p tauri-runtime-wry --doc
vendor\tauri-runtime-wry\check-windows.cmd check --offline --locked -p tauri-runtime-wry --features common-controls-v6,macos-private-api
vendor\tauri-runtime-wry\check-windows.cmd test --offline --locked --release -p tauri-runtime-wry --lib windows_tests::windows_target_lifecycle -- --nocapture --test-threads=1
vendor\tauri-runtime-wry\check-windows.cmd test --offline --locked --release -p tauri-runtime-wry --features devtools --doc
cargo tree --offline --locked -i tauri-runtime-wry -e features
```

Use the separate release lib/doc commands above. An aggregate `test --release`
without devtools passes the native tests but fails rustdoc's crate checking with
E0407: rustdoc selects the upstream debug-assertions-gated devtools methods while
the release tauri-runtime dependency omits them. RUSTDOCFLAGS disabling debug
assertions did not fix that on this toolchain. Explicit devtools for the doc-only
invocation aligns both existing trait surfaces; all three release doctests then
pass. This does not change the app's enabled features or production code.

The negative check below must fail with the explicit Windows tracing diagnostic,
not a dependency-resolution or compiler setup error:

```text
vendor\tauri-runtime-wry\check-windows.cmd check --offline --locked -p tauri-runtime-wry --features tracing
```

Release tests with `common-controls-v6` also need the activation manifest normally
embedded by Tauri's application build. Without it, the standalone test EXE fails
before main with 0xc0000139 because upstream release dialogs import
TaskDialogIndirect. The following PowerShell setup supplies that manifest only
to test builds; it does not change the runtime source or application build:

```powershell
$env:CARGO_ENCODED_RUSTFLAGS = @(
  '-C', 'link-arg=/MANIFEST:EMBED',
  '-C', "link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
) -join [char]31
.\vendor\tauri-runtime-wry\check-windows.cmd test --offline --locked --release -p tauri-runtime-wry --features common-controls-v6,macos-private-api --lib windows_tests::windows_target_lifecycle -- --nocapture --test-threads=1
```

Use that environment only for the package test invocation, not an application
build. Nine native subprocess scenarios pass with these release/app feature
settings. The same scenarios also run with the package's default features.

The workspace root owns the crates.io path patch and lockfile. The vendor is a
workspace member so package-only feature tests can use that same lock; explicit
default-members preserve the original application/helper build selection.
Root Cargo.lock removes this package's source/checksum and records its existing
optional tracing edge plus wry's existing optional tracing edge, as required by
Cargo's workspace-member feature resolution. No package/version is added or
updated and this does not enable runtime tracing in the app. The vendor's
published Cargo.lock is preserved as upstream provenance and is not the
application workspace lockfile.

## Local Verification And Review

Verified on Windows x64 with rustc/Cargo 1.97.1 and the VS 2022 Build Tools
vcvars64 environment. Package checks, nine native scenarios in debug and
release/app-feature configurations, and three compile-fail doctests passed.
The Windows tracing negative check reached and failed at the explicit guard.
The app's cuda/custom-protocol dependency tree resolves this vendored package
with no runtime tracing enabled. All 21 other upstream files, including both
licenses and manifests, were hash-checked byte-identical to the cached source;
the archive itself matches the SHA-256 above.

Independent read-only reviews found no new blocker in the bounded
target-ownership fix. Its conditional concern about WindowDispatch monitor
payload ownership was resolved by checking Tao's Windows isize-only handle;
worker window-monitor and inline-panic regressions were added. Remaining
notes concern the preexisting getter cancellation/unsafe surfaces documented
above, not a new target Rc or display-lifetime issue.

The complete Meetily binary also built successfully using
`cargo build --offline --locked -p meetily --release --bin meetily --features cuda,custom-protocol`.
TypeScript and whitespace checks passed. No installed app was replaced and no
release was published during this verification.

Remove this override only after an upstream release provides equivalent
thread-affine target ownership, safe Windows display handles, and unwind-safe
destruction, and these regressions pass against that release.
