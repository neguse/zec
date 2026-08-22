# PoC graduation contract

Status: In progress (2026-08-22)

## Meaning of graduation

PoC卒業は、エディタの機能数が増えたことではなく、Zedの編集コアを使うCUIの
基盤が「データを壊さず、文書サイズに対して設計上スケールし、自動検証と再現ができる」
状態になったことを指す。そこから先は機能PoCではなく、alpha editorの開発として扱う。

当面の対象はLinuxの対応terminal上でのsingle-process実行とする。LSP、プラグイン、
mouseの高度なselection、検索optionなどの機能完備は卒業条件に含めない。

## Gates

### G1. Zed is the sole editing authority

Pass.

- text、cursor、selection、transaction、undo、dirty stateはZedの`Editor` / `Buffer`が所有する。
- file open/save/save-as/reloadはZedの`RealFs -> WorktreeStore -> BufferStore`を通す。
- zecはterminal event、viewport、status/promptと、UTF-8 byte座標からterminal cell座標への
  変換だけを所有する。
- この境界を越えてEditorの振る舞いを複製する機能は、Zed側の公開APIを作るまで入れない。

### G2. Data and terminal lifecycle are safe

Fail.

- existing/new/scratch fileのopen/save/save-as、dirty tabのclose/quit保護、disk外部変更の
  auto reloadとconflict保護を持つ。
- reloadやreplaceもZedのtransactionであり、undo可能である。
- normal exitとerror pathでraw mode、alternate screen、mouse capture、bracketed pasteを復元する。
- 保護確認は別のinputが入った時点で解除し、古い確認状態を非表示で保持しない。

未達はcatch可能な`SIGTERM` / `SIGHUP`である。現在はprocessがそのまま終了し、
`TerminalSession::Drop`が走らないためterminal modeを残し得る。signalをevent loopの通常終了へ
変換し、PTY上で復元を自動検証するまでPassにしない。`SIGKILL`やmachine crashはprocess側で
cleanup不可能なので対象外とする。

また、固定中のZed revisionの`RealFs::save`はexisting fileをtruncateしてからRopeをstreamし、
atomic renameや`fsync`は行わない。zecが作った回帰ではないが、power lossやwrite途中errorで
disk原本がpartialになるproduction riskとして残る。PoC卒業ではZedの保存経路を迂回せず、
save failure後もBufferがdirtyのままであることを自動検証する。atomic/durable saveは
production-readyの別blockerとし、Zed upstream修正または明示的なforkなしにzec側へ保存を
再実装しない。

### G3. Per-frame work is bounded by the viewport

Fail.

現在の`capture_editor`は毎frame、全display rowのtext、row info、syntax chunk、
background highlightを取り直す。これは入力1回のコストをdocument全体のサイズに比例させるため、
卒業のblockerとする。

Pass条件:

- 通常のrepaintでZedから取得するline/row info/highlightはterminal本文の表示範囲と
  固定overscan以内に限る。
- `RenderSnapshot`はdocument全行の`String`やstyle vectorを保持しない。
- 100,000行fixtureでもcapture行数がterminal高で上限づけられることを自動testで固定する。
- wall-clockの参考計測は残すが、CIの合否はマシン速度ではなく処理行数で決める。
- redraw notificationはbounded/coalescedにし、解析eventの連打で無限にmemoryを増やさない。

### G4. The real binary has automated PTY acceptance tests

Fail.

現在のunit/headless testはZed APIとrendererを検証しているが、Crossterm入力から実binary、
filesystem、terminal cleanupまでの確認は手動PTYに依存している。

Pass条件:

- integration testが実際の`zec` binaryをPTY内で起動する。
- 最低限、insert/selection replacement/undo/saveのfile bytes、Unicode入力、resize後の継続操作、
  dirty exit保護、save failure後のdirty保持、normal exitと`SIGTERM` / `SIGHUP`後のterminal mode復元を
  black-boxで確認する。
- 各waitはdeadline付きで、固定sleepや無限blockを使わない。
- test失敗時はchild processを確実に終了し、開発者のterminalを変更しない。

### G5. A clean checkout is reproducible

Fail.

Zed revision、Rust toolchain、`Cargo.lock`は固定済みだが、repository自身にCIがない。
現時点のsource cache済み環境でfresh targetへの`cargo build --locked`は4分47秒、targetが
約9.4 GiB、debug binaryが約1.9 GBだった。Zedの大きなdependency graphは前提だが、
CIで同じtargetを重複buildしたり、runnerのdisk不足を起こす構成は卒業条件を満たさない。

Pass条件:

- Linux CIがclean checkoutから`cargo fmt --check`、build、unit/headless test、PTY acceptance、
  `--smoke`を実行する。
- commandは`--locked`を使い、Zedの検証revisionが意図せず変更されない。
- build/test/smokeは1つのtarget directoryを共有し、CI用のdebug info/incremental設定を含めて
  標準Linux runnerのdiskとtimeout内で実際に完走する。
- 卒業時に同じcommand setをlocalで実行し、結果を本documentに記録する。

## Execution order

1. G3: viewport-bounded captureとbounded redrawへ変更し、100,000行の構造的な回帰testを追加する。
2. G2/G4: catch可能signalを通常終了へ渡し、actual binaryのPTY integration harnessと
   core acceptance matrixを追加する。
3. G5: 同じmatrixをclean Linux CIに接続する。
4. 全gateを1回の検証で通し、statusを`Graduated`へ変更する。

この間、卒業gateを直接進めない機能追加は行わない。
