# PoC graduation contract

Status: In progress (2026-08-22)

## Meaning of graduation

PoC卒業は、エディタの機能数が増えたことではなく、Zedの編集コアを使うCUIの
基盤が「データを壊さず、文書サイズに対して設計上スケールし、自動検証と再現ができる」
状態になったことを指す。そこから先は機能PoCではなく、alpha editorの開発として扱う。

当面の対象はLinuxの対応terminal上でのsingle-process実行とする。LSP、プラグイン、
mouseの高度なselection、検索optionなどの機能完備は卒業条件に含めない。
性能gateの対象は、個々のdisplay lineが通常のsource code程度の長さで、行数が大きい文書とする。
数MBが1行に入るpathologicalな文書は、現在のZed公開APIではvisible columnだけを取得できず、
1行全体のmaterializeとgrapheme走査が残るため別の既知制約とする。この制約は黙ってPass扱いせず、
下記G3に残す。

## Gates

### G1. Zed is the sole editing authority

Fail.

- text、cursor、selection、transaction、undo、dirty stateはZedの`Editor` / `Buffer`が所有する。
- file open/save/save-as/reloadはZedの`RealFs -> WorktreeStore -> BufferStore`を通す。
- zecはterminal event、viewport、status/promptと、UTF-8 byte座標からterminal cell座標への
  変換だけを所有する。
- この境界を越えてEditorの振る舞いを複製する機能は、Zed側の公開APIを作るまで入れない。

本文や編集stateの境界は守れているが、`OpenDocument` がfile path/labelをcacheし、
save/reload/quit判定にも使っている。Save As成功時以外に同期しないため、外部rename/delete後に
ZedのBuffer file identityと不一致になる。pathとdisk stateをBufferからderiveし、キャッシュを
source of truthにしない構成へ直すまでPassにしない。

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

Pass (2026-08-23).

Ratatuiの`try_draw` callback内で実際のframe areaを取得し、cursor followとviewport clampを
先に確定してから、そのframeで必要なdisplay rowだけをZedから取得する。text、row info、
syntax chunkは`[top_row, top_row + body_height)`に限定し、background highlightも同じ範囲の
Anchorだけを問い合わせる。resize直後も古い高さでcaptureしたsnapshotを新しいframeへ描かない。

`RenderSnapshot`はglobalな`first_row` / `total_rows` / cursor位置と、可視行だけの
`lines` / `line_numbers` / `line_styles`を持つ。renderer、mouse hit test、selection、
backgroundはglobal rowからrow-local vectorへ変換する。cursorが画面外でもstatus用のrow infoは保持する。

80x24 terminalと100,000行fixtureのheadless testで、先頭・中央・末尾のいずれも本文23行だけを
保持し、`total_rows`は全文を表すことを固定した。terminal event channelはcapacity 1とし、
重複Redrawは`try_send`でcoalesceし、入力はreader thread側でbackpressureする。

既知制約は、巨大な単一display lineと、query変更時の全文検索、match数に比例するZed検索navigation
である。必要ならvisible-column APIまたは検索用hookをZed側へ提案する。

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

1. G3 (done 2026-08-23): viewport-bounded capture、bounded redraw、100,000行の構造test。
2. G1/G2: file identityをZed Bufferからderiveし、外部rename/delete時の保護を固定する。
   catch可能signalもevent loopの通常終了へ渡す。
3. G4: actual binaryのPTY integration harnessとcore acceptance matrixを追加する。
4. G5: 同じmatrixをclean Linux CIに接続する。
5. 全gateを1回の検証で通し、statusを`Graduated`へ変更する。

この間、卒業gateを直接進めない機能追加は行わない。
