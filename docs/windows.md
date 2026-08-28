# Windows サポート

zecはWindowsをfirst-classのnative targetとして扱う。GPUIは非表示windowを作るnative platform、
外側のTUIはWindows Terminal等のVT対応console、integrated terminalはConPTYで動く。

## ビルド要件

- MSVC toolchain(rust-toolchain.tomlのpinned Rust)
- Visual Studioの「MSVC C++ x64/x86 Spectre緩和ライブラリ」component。
  `languages`(Zed)→`pet`経由の`msvc_spectre_libs`が要求し、無いとbuild scriptがpanicする。
  GitHub hosted runnerには既に入っている。

## 分離とデータ配置

`ZEC_DATA_DIR`を設定すると、Zed側のuser directory一式(config/data/logs/DB)が
`$ZEC_DATA_DIR`配下(configは`$ZEC_DATA_DIR/config`)へ全platform同一の仕組みでredirectされる。
test harnessとactual-binary gateはこれで分離する。XDG_*はUnix専用の残余経路のみを覆う。

workspace sessionの既定保存先は、`ZEC_SESSION_DIR` > `ZEC_DATA_DIR/state/workspaces` >
(Unix: `XDG_STATE_HOME`または`~/.local/state`、Windows: `LOCALAPPDATA`)`/zec/workspaces`。

## test harness

`PtySession`(src/bin/alpha_1_support)はportable-ptyのnative PTY(Unix: pty、Windows: ConPTY)で
実binaryを駆動する。platform差は`TerminalBaseline`に集約している:

- Unix: termios snapshotでraw-mode遷移と復元をkernel状態として証明する
- Windows: ConPTYに等価物が無いため、emitされたVT列(alternate screen進入とcleanup escapes)で判定する

signal(SIGTERM/SIGHUP/SIGTSTP)・process group・/proc metricsを使うAPIはUnix専用のまま
`#[cfg(unix)]`で残る。Windowsにはprocess groupとjob controlのTUI慣行が存在しないため、
これらのcaseはUnix evidenceとしてのみ意味を持つ。VmHWM相当はWindowsではPeakWorkingSetSize。

## Windowsで走る検証

- `cargo test --locked`の全integration test(alpha_2_lsp / alpha_2_settings / alpha_2_failures /
  update_cli / parity_contract / repository / alpha_3_*)。feature gateは不要。
- `alpha_2_acceptance` / `alpha_2_bench`はConPTY上でbuild・実行できる。
- `pty_acceptance`は`#![cfg(target_os = "linux")]`のまま。termios/signal前提のcaseを分離した上での
  case単位のWindows有効化が次の課題。
- `alpha_1_*` / `alpha_3_acceptance` / `alpha_3_bench`は`alpha-1-linux` featureで据え置き。
  Alpha 1はfixture manifestがUnixのmode/symlinkを契約に含むため、契約の再設計なしには移植できない。

`zec update apply`はWindowsでは実行中executableを置換せず、documented errorで拒否する
(tests/update_cli.rsが両platformの挙動を検証する)。
