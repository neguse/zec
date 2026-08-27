# Alpha 3 contract: Terminal Workspace

Contract status: Implemented candidate; hosted evidence pending (2026-08-26)

Alpha 3は、Alpha 2までの単一pane向けevent loopを、pane、tab、dock、panel、overlay、focus、
navigation historyを一元管理するTerminal Workspaceへ移行する。ZedのBuffer、Editor、Project、
MultiBufferを引き続きauthorityとし、端末側はlayoutとpresentationだけを所有する。

この文書のcase ID、sample数、上限、artifact規則を満たすhosted candidateとevidence-only direct childが
成功するまで、parity台帳のAlpha 3 capabilityは`candidate`に留める。

実装済みcandidateでは`alpha_3_acceptance`、`alpha_3_bench`、4つのnamed integration target、
前段回帰runner、artifact/evidence verifier、`Alpha 3` workflowまで存在する。開発用の1-run
actual-binary PTY matrixは19/19、縮小performance matrixは全7 metricを完走している。ただし
`ZEC_ALPHA3_DEV_RUNS` / `ZEC_ALPHA3_DEV_SAMPLES`を使ったreportはverifierがcanonical evidenceとして
拒否する。`candidate`から`verified`への昇格条件は、この文書どおりの361 caseと固定scale benchmarkを
環境変数による短縮なしでhosted runner上に通し、promotion commitを検証することである。

## Outcome

fresh repositoryを`zec DIRECTORY`で開き、consoleだけで次を完結できる状態をAlpha 3とする。

- paneの水平・垂直split、方向focus、item移動、ratio変更、tab preview/pin/reorder
- left/right/bottom dockとproject/outline/diagnostics panel
- project treeからのopen/create/rename/delete/copy、競合確認、trust/path境界
- regex/case/whole-word/include/exclude付きproject search、preview MultiBuffer、atomic replace
- outline、breadcrumbs、go-to-symbol、back/forward、同一Buffer identityを保つnavigation
- fold、soft wrap、複数selection、mouse drag/double/triple click、scroll/cursor projection
- clean exitとcrash後のworkspace/session復元、破損sessionの隔離、unsaved recovery
- terminal capability検出と、識別不能key/mouse/focus eventの明示fallback

## Single decision rule

candidateはGitHub-hosted Ubuntu 24.04 x86_64のclean checkoutで次を順に1回だけ実行し、すべてexit 0に
ならなければならない。retry、case filter、sample除外、手動確認による代替Passを認めない。

```sh
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TERM=xterm-256color
cargo metadata --locked --format-version 1 --no-deps >/dev/null
cargo fmt --all -- --check
cargo test --locked --release --features alpha-1-linux \
  --bin zec -- --test-threads=1
cargo test --locked --release --features alpha-1-linux \
  --test parity_contract --test pty_acceptance \
  --test alpha_2_lsp --test alpha_2_settings --test alpha_2_failures \
  --test alpha_3_workspace --test alpha_3_project_panel \
  --test alpha_3_search --test alpha_3_session -- --test-threads=1
cargo build --locked --release --features alpha-1-linux \
  --bin zec --bin alpha_1_acceptance --bin alpha_1_bench \
  --bin alpha_2_fixture_lsp --bin alpha_2_acceptance --bin alpha_2_bench \
  --bin alpha_3_acceptance --bin alpha_3_bench
./script/run-alpha-1-and-2-gates target/alpha-3/regression
timeout --signal=TERM --kill-after=5s 60m \
  ./target/release/alpha_3_acceptance --zec ./target/release/zec \
  --assert --report target/alpha-3/acceptance.json
./target/release/alpha_3_acceptance \
  --verify-report target/alpha-3/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/alpha_3_bench --zec ./target/release/zec \
  --assert --report target/alpha-3/benchmark.json
./target/release/alpha_3_bench \
  --verify-report target/alpha-3/benchmark.json
(cd target/alpha-3 && find . -type f ! -name SHA256SUMS -print0 \
  | sort -z | xargs -0 sha256sum > SHA256SUMS)
./script/verify-alpha-3-artifact target/alpha-3
```

未実装binary、script、report、case、artifactが1つでも存在しなければFailとする。このblockを実装途中の
短縮commandへ置換してはならない。

## C0. Prior milestone regression and ledger

- Alpha 1の186 acceptance、benchmark、PoC 94 IDを同じzec binaryで再実行する。
- Alpha 2の341 acceptance、benchmark、34+5 embedded evidenceを同じzec binaryで再実行する。
- Alpha 1/2のcanonical evidence fileを検証し、前段capabilityを`verified`として台帳へ進める。
- pinned Zed revision、capability ID、delivery mode、milestone、evidence pathをparity testで固定する。

## C1. One Terminal Workspace authority

pane、tab、dock、panel、overlay、focus、navigation historyを1つの`WorkspaceModel` reducerが所有する。

- すべてのZed Editor/MultiBuffer itemにprocess内で一意かつsession上で安定したitem identityを与える。
- layoutはbinary split tree、paneはordered item list、dockはordered panel listとして保持する。
- 同じitemを複数paneへ重複登録せず、既存itemを開く操作は既存identityへfocusする。
- pane close時はsplit treeをcollapseし、active pane/item/focusを必ず有効な対象へ移す。
- overlayはLIFO stackとし、最上位だけが入力を受け、close後は直下overlayまたは元focusへ戻す。
- reducer transactionごとにlayout leaf、pane map、item集合、focus、preview/pinのinvariantを検査できる。
- rendererとsession writerはimmutable workspace snapshotだけを読み、frame中にstateを変更しない。

Zed Buffer/Editorの本文、selection、undo、DisplayMapをworkspace modelへ複製してはならない。

## C2. Pane, tabs, docks, and responsive rendering

- horizontal/vertical split、方向focus、active itemの隣接pane移動、split ratio変更を提供する。
- terminal resize後もratioを保ち、0-cell paneを作らず、狭すぎる場合はactive paneを優先して明示縮退する。
- tabはpreview、pin、close、reorder、next/previousを持つ。dirty previewは自動置換せずpinへ昇格する。
- left/right/bottom dockはvisibility、cell size、active panelを保持し、toggle前のsizeを復元する。
- Editor pane、tab strip、dock、status、overlayの描画領域は重ならず、cursorはfocus中Editorだけに出す。
- split/dockの境界をmouse dragでき、mouse非対応時はcommand paletteとkey bindingで同じ操作を行える。
- 4 pane、各20 tab、10万行bufferでもcaptureはvisible viewportだけにboundedである。

## C3. Project panel and file operations

Project panelはZed WorktreeStoreのentry identityとscan updateをauthorityにする。

- directory expand/collapse、filter、selection、reveal-active-file、ignored/hidden表示切替を提供する。
- keyboard/mouseでfile open、preview/pin、new file/directory、rename、delete、copy、duplicateを操作できる。
- create/rename/delete/copyはpreviewと明示確認を経てProject/Worktree APIへ渡し、成功後のscanで表示を更新する。
- dirty/open Bufferのrenameは同じBuffer identityとundoを保ち、deleteは本文を保持して既存discard guardへ接続する。
- repository外、`..`、absolute escape、symlink escape、FIFO/socket/device、case-fold衝突を拒否する。
- untrusted worktreeのmutationとprocess起動はtrust promptで止め、cancel時はdisk/Bufferを変更しない。
- watcher eventがrename/delete/createの途中順序で届いてもduplicate entryやstale selectionを残さない。

## C4. Complete project search and replace

既存のliteral/case-sensitive searchをZed search query semanticsへ拡張する。

- literal/regex、case-sensitive、whole-word、include glob、exclude glob、open-buffer-onlyを切り替えられる。
- query optionとreplacementは履歴を持ち、invalid regex/globは本文を変えずprompt内に表示する。
- resultはpath/position順のMultiBuffer excerptとし、context行、match highlight、折畳みを持つ。
- dirty open Bufferをdiskより優先し、同一Buffer aliasをdedupeし、ignored/binary/special file規則を保つ。
- replace-one、replace-file、replace-allはpreview diffと対象件数を表示し、accept後だけ1 ProjectTransactionで適用する。
- apply直前にBuffer generationとdisk fingerprintを再検査し、stale/overlap/conflict時は全体を中止する。
- save、undo、redoは全source Bufferを一貫して扱い、部分適用やsilent skipをしない。
- cancel、query変更、pane/project close後のstale generationを描画・適用しない。

## C5. Outline, breadcrumbs, and navigation

- Zed language outline/symbol authorityからdocument outlineを構築し、階層、kind、range、selection rangeを表示する。
- outline filter、expand/collapse、follow-cursor、選択移動を提供し、stale parse結果を捨てる。
- status上のbreadcrumbsはworktree pathとsymbol ancestryをterminal幅で省略し、各segmentからpickerを開ける。
- go-to-symbol、definition、type definition、references、diagnostics、search resultは共通navigation transactionを使う。
- back/forwardはpane/item/path/selection/viewportを復元し、削除済みtargetは履歴を壊さずskip理由を表示する。
- navigationで開いた一時itemはpreview、編集されたitemはpinとなり、同じBufferを複製しない。

## C6. Advanced editor presentation and input

- Zed fold actionsとDisplayMapを使い、fold/unfold/toggle/all、fold marker、cursor revealを提供する。
- soft wrapはterminal cell幅をZed wrap boundaryへ渡し、wide/combining/emoji graphemeでcell位置を壊さない。
- 複数selection/cursorの追加、上下追加、select-next/all-occurrencesをZed Editor actionで実行し、全cursorを描画する。
- mouse drag、double-click word、triple-click line、Shift extend、Ctrl/Alt add-selectionをterminal event能力に応じて扱う。
- rectangular selection、inlay、inline diagnostic、indent guide、whitespace表示はZed snapshotからterminalへ投影する。
- Kitty keyboard protocol / modifyOtherKeys / focus event / mouse motion / OSC 52 / OSC 8を検出し、利用可否をstatusへ出す。
- 区別不能な操作にはcommand palette経由のportable routeを持たせ、入力をsilent dropしない。

## C7. Session and crash recovery

session fileはversion、repository identity、layout、pane/tab order、active item、dock、panel、cursor/selection、
viewport、fold、search/navigation historyをatomic writeで保存する。

- clean quit後の再起動でfile-backed itemとlayoutをexactに復元する。
- scratch/dirty Bufferはcontent-addressed recovery blobへ保存し、元pathへ自動上書きせずrecovered itemとして開く。
- save成功後は対応blobを回収し、他sessionや新しいgenerationのblobを削除しない。
- process crash、SIGKILL、partial session writeをfixtureで発生させ、最後のatomic snapshotとrecovery journalから復元する。
- schema/version/hash不正、truncated JSON、巨大session、symlink session pathは隔離し、fresh workspaceで起動してerrorを表示する。
- 同じrepositoryを2 processで開く場合はlock/generationを使い、後発processが先発sessionを破壊しない。
- restore対象がrename/deleteされた場合はworktree identityで追跡し、不明ならmissing itemとして明示する。

## C8. Failure, security, and lifecycle matrix

次の9 scenarioを各20 fresh processで実行する。

1. corrupt/truncated session
2. project-panel symlink escape
3. file-operation permission denied
4. dirty delete/rename conflict
5. replace fingerprint conflict
6. terminal 1x1からのresize storm
7. watcher overflow/rescan reorder
8. stale outline/panel/search generation
9. crash during recovery journal commit

各caseは既存Buffer/undo/dirtyを保持し、UIが250 ms以内に再応答し、終了時5秒以内にchild、watcher、PTY、
lock fileを回収する。入力、filesystem mutation、session write、external processには単調IDを付け、requestと
applyの順序・欠落・重複をreportへ保存する。

## C9. Acceptance and performance envelope

`alpha_3_acceptance`は次の361 IDを完全一致で実行する。

- `C1_WORKSPACE_MODEL` 1件
- `C2_LAYOUT_*`、`C2_TABS_*`、`C3_PROJECT_PANEL_*`、`C4_SEARCH_REPLACE_*`、
  `C5_NAV_OUTLINE_*`、`C6_ADVANCED_EDITOR_*`、`C7_SESSION_RESTORE_*`、
  `C7_CRASH_RECOVERY_*`、`C8_CAPABILITY_FALLBACK_*`を各20件
- 9 failure scenarioを`C8_<SCENARIO>_*`として各20件

benchmarkはrelease actual binary、120x40 PTY、warmupを除外しないraw sampleで次を満たす。

- 4-pane redraw: 10 warm-up後100 samples、p95 16 ms以下、max 50 ms以下
- project panel initial ready (10,000 entries): 20 samples、p95 750 ms以下、max 1,500 ms以下
- outline update (10,000 symbols): 100 samples、p95 100 ms以下、max 250 ms以下
- regex search first visible result (10,000 files): 100 samples、p95 150 ms以下、max 500 ms以下
- 1,000-file replace preview: 20 samples、p95 750 ms以下、max 2,000 ms以下
- back/forward apply: 10 warm-up後500 samples、p95 16 ms以下、max 50 ms以下
- 20-pane/session restore: 20 samples、p95 1,500 ms以下、max 3,000 ms以下
- 10万行x4 pane、10,000 tree、10,000 results＋MultiBuffer、10,000 symbolsの各独立scenarioで
  VmHWMは1,879,048,192 bytes以下

report verifierはnearest-rank統計、raw sample数、全case ID、全correlation列、binary SHA、environment、
artifact内manifest/session/event traceを再計算する。

`alpha_3_bench`のworkloadはreportにも固定値として保存し、10,000 tree files、10,000 outline symbols、
1,000 replacement sources、100,000-line source、20-pane sessionのいずれかが小さければ検証時にFailとする。
4-paneの狭幅縮退中は長いstatus文言に依存せず、dockの出現・消滅、実excerpt、検索総数をVT screenから
判定する。計測sampleは入力を書いた時点から、その結果を含むproduction frameをPTY readerが完了した
時点までである。regex latencyは10,000 filesを走査して1件に一致するregexで測り、容量scenarioは別の
actual-binary processで`1/10000`の完了と、その全結果から作ったMultiBufferを検証する。

## C10. Hosted evidence

Alpha 1/2と同じcandidate `C` / promotion `P`規則を使う。

1. default branchへpushした`C`の`Alpha 3 gate`をrun attempt 1、retry 0で成功させる。
2. regression 4 report、Alpha 3 acceptance/benchmark、PTY/event/session/manifestsを1 artifactへ保存する。
3. `P`は`docs/alpha-3-evidence.json`だけを追加する`C`のdirect childとする。
4. evidence jobはGitHub APIからrun/job/artifactの一意性、SHA、event、attempt、conclusion、digestを読戻す。
5. downloaded artifactを`verify-alpha-3-artifact`で再検証し、default branchのevidence job成功だけをPassとする。

artifactは次のself-contained layoutを持つ。

```text
alpha-3/
  acceptance.json
  acceptance-artifacts/{event-trace,manifest,session-trace}.json
  benchmark.json
  benchmark-artifacts/{latency-trace,memory-observations,session-generation,workload-manifest}.json
  regression/alpha-2/
    acceptance.json
    benchmark.json
    alpha-1/{acceptance,benchmark}.json
    ... Alpha 1/2 embedded evidence and SHA256SUMS
  SHA256SUMS
```

root verifierは全embedded fileのsize/SHA-256、361 caseの順序、860 benchmark correlation ID、nearest-rank
統計、全binary digestの一致を再計算し、nested Alpha 2 verifierへ前段artifactを再検証させる。

## Explicit exclusions from Alpha 3 gate

次はparity scopeから除外せず後続milestoneへ送る。

- Git staging/commit/branch/remote UI、integrated terminal、tasks/tests
- DAP debugger、REPL/notebook、process/session recovery
- extension/theme/icon/package/update、remote development、platform package
- AI/ACP/MCP/edit prediction、collaboration、voice/screen-share bridge
