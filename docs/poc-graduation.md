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

Pass (2026-08-23).

- text、cursor、selection、transaction、undo、dirty stateはZedの`Editor` / `Buffer`が所有する。
- file open/save/save-as/reloadはZedの`RealFs -> WorktreeStore -> BufferStore`を通す。
- zecはterminal event、viewport、status/promptと、UTF-8 byte座標からterminal cell座標への
  変換だけを所有する。
- この境界を越えてEditorの振る舞いを複製する機能は、Zed側の公開APIを作るまで入れない。

`OpenDocument`はBufferとscratch用labelだけを保持し、path、disk state、dirty、conflictは
毎回Zedの`Buffer::file()`から導出する。既存の最寄りnon-root directoryをinvisible worktreeにし、
外部rename後も同じBuffer Entityと新pathを追跡する。cleanな外部deleteはZed上`is_dirty=false`
だが、`DiskState::Deleted`を明示的にclose/quit保護と`!`表示へ含める。

headless回帰試験はrename後のpath/dedupe、編集・新pathへのsave、旧path非再作成、delete後の
本文保持と破棄保護を実filesystem watcher込みで確認する。

### G2. Data and terminal lifecycle are safe

Pass (2026-08-23).

- existing/new/scratch fileのopen/save/save-as、dirty tabのclose/quit保護、disk外部変更の
  auto reloadとconflict保護を持つ。
- reloadやreplaceもZedのtransactionであり、undo可能である。
- normal exitとerror pathでraw mode、alternate screen、mouse capture、bracketed pasteを復元する。
- 保護確認は別のinputが入った時点で解除し、古い確認状態を非表示で保持しない。

catch可能な`SIGTERM` / `SIGHUP`はsignal handlerからatomic flagだけを更新し、terminal readerが
通常のevent loop終了へ渡す。reader停止後、raw mode等を復元してからhandlerを解除する。両signalで
起動前後のtermios完全一致をG4のactual-binary PTY試験で固定した。normal exit、dirty discard、
`SIGTERM`、`SIGHUP`の全経路でalternate screen、mouse capture、bracketed paste、cursor stateも
復元する。`SIGKILL`やmachine crashはprocess側でcleanup不可能なので対象外とする。

actual-binary試験はZed `BufferStore`によるexact file bytesの保存、dirty quit保護、途中componentを
通常fileにした`ENOTDIR` save failure後の本文・dirty・disk原本保持もblack-boxで確認する。

また、固定中のZed revisionの`RealFs::save`はexisting fileをtruncateしてからRopeをstreamし、
atomic renameや`fsync`は行わない。zecが作った回帰ではないが、power lossやwrite途中errorで
disk原本がpartialになるproduction riskとして残る。Zedの保存先をdirectoryへ置換した
headless failure試験で、error後もBuffer本文とdirty、退避したdisk原本が保持されることは固定した。
G4のactual-binary試験でも同じ保護を固定した。PoC卒業ではZedの保存経路を迂回しない。
atomic/durable saveは
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

Pass (2026-08-23).

`tests/pty_acceptance.rs`は`CARGO_BIN_EXE_zec`をcontrolling PTY内で直接起動し、raw ANSI outputを
`vt100`でsemantic screenへ復元して次の4 subprocessを逐次検証する。

- Unicode insert、`Ctrl-A` selection replacement、Zed undo、PTY resize後の継続編集、Zed
  `BufferStore` save、exact file bytes、dirty quit保護、normal exit。
- 通常fileをpath途中へ置くことで決定的に発生させた`ENOTDIR` save failure、本文・dirty・disk原本保持、
  dirty discard保護。
- direct child PIDへの`SIGTERM`と`SIGHUP`がsignalによる即死ではなくzecの通常error exitを通ること。
- 全経路で起動前後のtermios完全一致と、alternate screen、mouse、bracketed paste、cursor、application
  modeの解除。

各waitは画面またはraw outputのpredicateとdeadlineで進み、固定sleepを使わない。親側slave FDはspawn後
すぐ閉じ、EOFはcleanup bytesと競合しない状態として扱う。timeoutやpanic時はRAIIでchildをkill/reapし、
writer、master、receiverを閉じてからreader threadをjoinする。

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
2. G1 (done 2026-08-23): file identityをZed Bufferからderiveし、外部rename/delete時の保護を固定。
   G2 (done 2026-08-23): data guardとcatch可能signalをactual-binary PTYで自動検証する。
3. G4 (done 2026-08-23): actual binaryのPTY integration harnessとcore acceptance matrixを追加。
4. G5: 同じmatrixをclean Linux CIに接続する。
5. 全gateを1回の検証で通し、statusを`Graduated`へ変更する。

この間、卒業gateを直接進めない機能追加は行わない。
