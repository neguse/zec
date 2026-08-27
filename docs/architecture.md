# Architecture decisions

Status: Accepted (2026-08-21)

## Goal

Zed の `editor::Editor` を編集機能の本体として使い、端末固有の入出力だけを追加する。
Editor、Buffer、selection、undo、keymap は再実装しない。

headless の挿入・undo PoCに加え、plain textの端末表示、キー入力、移動、undo/redo、
selection表示、論理行番号、native languageのsyntax highlight、paste、resize、
terminal viewportのscroll、
本文のmouse clickによるcaret移動、
現在directoryまたは明示した`DIRECTORY`をrootとするrepository mode、1つのZed Worktreeと
`RepositoryIndex`によるfile discovery、`Ctrl-P`のQuick Open、`Alt-F`のproject-wide search、
実ファイルのopen/save/save-as、`Ctrl-F`のBuffer内検索、go to line/column、terminal clipboard、dirty表示、
複数tab、実行中のtab open/close、終了時の端末復元まで実装済み。

## Repository strategy

Zed monorepoの fork ではなく、独立した binary crate から Zed の各 crate を Git
dependency として使う。検証対象が勝手に変わらないよう revision は `Cargo.toml` で
固定する。Zed 側の private API が本当に必要になるまでは fork や本体変更を持たない。

repository modeではcanonicalizeした`Directory root`につき、rootと一致するvisibleなZed Worktreeを
1つだけ作る。scan完了後のWorktree snapshotからimmutableな`RepositoryIndex`を構築し、zec独自の
filesystem walkやfileごとのworktreeは作らない。Zedのignore/external判定とcanonical file identityを
使うため、symlink aliasも同じfileへ集約される。direct `FILE...` modeは独立した起動経路として残す。

## Decision

構成は次の通りとする。

```text
crossterm Event
    -> channel
    -> GPUI foreground
    -> Zed Editor
    -> DisplaySnapshot
    -> zec の Ratatui Widget
    -> ratatui::Buffer
    -> CrosstermBackend
    -> terminal

DIRECTORY (no argsならcurrent directory)
    -> canonical RepositoryRoot
    -> Zed RealFs
    -> 1 visible Worktree
    -> immutable RepositoryIndex
       -> Ctrl-P Quick Open -> ProjectPath
       -> Alt-F -> Zed Search -> Buffer + Anchor range
    -> shared BufferStore
    -> Zed Buffer × N

FILE... (direct mode)
    -> Zed RealFs -> WorktreeStore -> shared BufferStore

Zed Buffer × N
    -> hidden Editor window × N
    -> tree-sitter parse / highlighted chunks
```

| Component | Responsibility |
| --- | --- |
| Zed Editor / Buffer | テキスト、カーソル、selection、編集 action、undo、dirty状態 |
| Zed Search / Buffer / Anchor | project-wide matchの探索、本文、match range |
| Zed BufferStore / WorktreeStore / RealFs | repository file集合とignore判定、ファイルのopen/save、encoding・改行・disk state |
| Zed Language / tree-sitter | language query、parse、syntax highlight range |
| GPUI headless | Zed の runtime と window/action context |
| Crossterm | raw mode、キー・paste・resize入力、端末への出力 |
| Ratatui | レイアウト、cell buffer、style、差分描画 |
| zec RepositoryRoot / RepositoryIndex | root identity、alias dedupe、Quick Openの決定的な表示index |
| zec | 端末イベント変換、Zed snapshot の cell 化、処理の接続 |

編集状態の source of truth は常に Zed とする。端末入力は Zed の action/input 経路へ
渡し、描画側は Zed の immutable な表示 snapshot を読むだけにする。

Ratatui は cell buffer、style、レイアウト、前後 frame の差分描画を既に提供するため
採用する。入力 loop は所有しないので、Crossterm で読んだイベントを zec が Zed の
入力へ変換する。

## Frame capture and redraw

通常のrepaintで文書全体をterminal snapshotへ複製しない。Ratatuiの`try_draw` callbackで
そのframeのareaを取得し、cursor followとdocument末尾へのclipを済ませてから、確定した
`[top_row, top_row + body_height)`だけを同じcallback内でZedの`DisplaySnapshot`から読む。
これによりresizeとcaptureの間でviewportがずれず、最初のframeが空になるraceも避ける。

`RenderSnapshot`のtext、line number、syntax styleは`first_row`から始まるrow-local vectorで、
`total_rows`、cursor、selection、background rangeはglobal display rowを使う。rendererとmouse
hit testだけがglobal rowをlocal indexへ変換する。全体の行数と最大行番号はZedのsummary API、
可視syntaxは`highlighted_chunks`のrow range、検索等の背景は可視範囲のAnchorを
`background_highlights_in_range`へ渡して取得する。terminal側でfoldやwrapを再計算しない。

terminal event channelはcapacity 1にする。重複したRedrawはdropしても次frameが最新snapshotを
読むため安全であり、key/paste/mouseはreader threadでbackpressureして順序を保つ。
`SIGTERM` / `SIGHUP`のhandlerはasync-signal-safeなatomic flag更新だけを行う。terminal readerが
その番号を通常の`TerminalEvent`へ変換し、GPUI loopを終了する。cleanupはreader threadをjoinし、
`TerminalSession`がraw mode、alternate screen、mouse capture、bracketed pasteを復元してから
signal handlerを解除する順に固定する。復元処理はescape出力とraw mode解除の片方が失敗しても
もう片方を必ず試し、明示的なrestore失敗は呼び出し元へ返す。


100,000行の構造testでは80x24の先頭・中央・末尾で保持行数が23に固定される。これは行数に対する
repaintの上限であり、全文検索そのものを定数時間にする主張ではない。またZedの公開text APIは
display row単位なので、数MBの単一行ではその行全体のmaterializeとterminal cell変換が残る。
必要になった時点で独自text modelを作らず、Zed側のvisible-column iterator/hookを検討する。

## Repository root, Quick Open, and Project Search

引数なしの`zec`はcurrent directory、`zec DIRECTORY`は明示したdirectoryをrepository rootにする。
rootはcanonical pathをidentityとし、同じrootの`.`、absolute path、symlink spellingが別repositoryや
別Worktreeにならないようにする。Zed Worktreeのscan完了を待ってから、そのsnapshotだけを
`RepositoryIndex`へ投影する。indexはrelative path、canonical identity、alias、`ProjectPath`を持つ
presentation用のimmutable snapshotであり、file本文、selection、undo、dirty stateは所有しない。

`Ctrl-P`のQuick Openは`RepositoryIndex`を絞り込み、決定的にrankした先頭100件から選択する。
`Enter`後はindexが保持するZed `ProjectPath`を`BufferStore`へ渡すため、同じfileやsymlink aliasを
別Bufferへ複製しない。Quick OpenとProject Searchはrepository modeだけの操作で、direct
`zec FILE...` modeのopen/save経路はそのまま残す。

`Alt-F`のProject Searchは`RepositoryIndex`の決定順file setとZed `SearchQuery`を使う
case-sensitiveなliteral検索である。実行ごとのworker数はGPUIのCPU数を1〜4へclampし、dispatch済み
fileもworker数の2倍までに制限する。closed fileのdisk prefilterとno-hitの決定順retireは固定
background worker/collector内で完結し、disk hitまたはopen bufferだけをforeground `AsyncApp`へ渡す。
open bufferはdisk prefilterを迂回して同一physical fileの全aliasを`BufferStore` snapshotから検索し、
closed fileのhitは`BufferStore`でopenする。disk判定はbinary除外以外はoptimizationに留め、最終authorityは
常にZed `BufferSnapshot`と`Anchor` rangeとする。
zecはsnapshotからpath、1-based line、BOMを除いたUnicode scalar column、previewへ投影し、canonical
file identityとrangeの重複を除いて決定順に並べる。sourceは5,000 matching filesまたは10,000 rangesの
exact limitまで保持し、5,001件目または10,001件目を検出した場合だけlower-bound flagを立てる。全hit数を
保持したままterminalへ表示するresultは先頭100件に制限し、`Enter`では保持していた同じBufferとAnchor
rangeへcaretを移動する。

queryを変更するたびにprompt-local generationを増やし、app-wide coordinatorは同時に実行する
project searchを最大2件、待機requestをlatest 1件に制限する。key editは旧runを直ちにcancelしてから
16 msのtrailing debounceでまとめ、bracketed pasteはatomicな完成queryとして直ちにeligibleにする。
debounce後のkey queryは実行中searchがある間も待機させるため、置換時のbackspace中間queryは2枠目を
占有せず、完成したpasteだけが予約した2枠目へ直ちに進める。

cancelはuser用signalを閉じ、file scan、queue待機、snapshot chunkの各境界をwakeする。source cap用の
internal stopとは分離し、user cancelだけをerrorとして返す。producerと固定workerはdropせず自然終了まで
joinし、app-wide coordinatorはfinish eventまで外側Task handleを保持する。pasteによるsupersede、空query、
`Esc`、prompt closeはいずれもreducer/debounce/pendingをlogical cancelするが、in-flight Taskはdropしない。
app teardownだけはcancel後に外側Taskをdetachし、そのTask自身がworker joinを完了する。

各requestはprompt session IDとgenerationの組で識別するため、古いcompletionがcapacity 1の
terminal channelへ既に入った後でpromptを開き直しても、新sessionへsuccess/errorをpublishしない。
promptが存在しない間のfinish eventもcoordinator自身は処理し、ack済みslotを必ず解放する。

Alpha 1 benchmarkはactual production binaryをPTYで操作し、VT parserが描画したstatusを下矢印で
1件ずつ進め、表示上限100件のpath、line、column、previewと順序をすべてspecと直接比較する。
結果一覧をfixture fileやtest-only interfaceから読み出さず、production search pathにも
test-only file hookを設けない。

## File I/O

repository modeでindexから選んだfileと、direct modeの`zec FILE...`はどちらも
Zedの `RealFs -> WorktreeStore -> BufferStore` で開く。`Project` 全体や手書きの
`std::fs::write` は使わない。これによりencoding、BOM、改行コード、保存version、
外部ファイル状態をZed側の実装に任せられる。

該当worktreeがなければ、pathを含む既存の最寄りnon-root directoryを非表示worktreeとして扱う。
これにより同じdirectory内の外部renameをZedのentry identityで追跡でき、未作成のnested Save As
pathも作成後に追跡対象になる。該当するdirectoryがfilesystem rootしかない場合だけ、root全体を
scan/watchせずファイル自身をsingle-file worktreeにする。directory worktreeは親配下を再帰的に
scan/watchするため、rename追跡と初期I/Oの交換条件である。

未作成パスもfile付きの `DiskState::New` Bufferになるため、編集後の `save_buffer` で新規作成できる。

`Ctrl-N`で作るscratchも裸の `Buffer::local` にはせず、最初から同じ `BufferStore` の
`create_local_buffer` で生成する。`Ctrl-S` ではterminal-ownedな1行promptから絶対化した
保存先を `find_or_create_worktree -> save_buffer_as` へ渡す。成功後は同じBuffer Entityに
fileが付き、Editor、selection、undo履歴、dirty versionを作り直さず通常の `save_buffer`
へ移行できる。LspStoreを初期化していないため、save-as後の拡張子に対するlanguage選択だけは
zecが明示的に再実行する。

Save Asの相対パスは起動時working directory基準で、shell expansionは行わない。既存の
regular fileは1回目のEnterで警告し、同じ入力で2回目のEnterを押した時だけ上書きする。
directory、FIFO、その他のspecial fileは拒否する。この確認は操作ミスを防ぐUI境界であり、
確認から保存までの外部filesystem raceを排除するatomic no-clobber保証ではない。
複数tabは1組のLanguageRegistry、RealFs、WorktreeStore、BufferStoreを共有する。同じ
ProjectPathのopenはBufferStoreが同じBuffer Entityへdedupeし、CLIの完全に同じpathも
事前に除外する。Save As先が別tabのBufferとして既にopenなら、同じdisk pathに2 Bufferを
結びつけず明示的に拒否する。

Zed既定keymapの `Ctrl-S` はWorkspace actionだが、このbinaryのrootはEditorなので
保存handlerがない。そのため `Ctrl-S` だけCLI側で捕捉して `BufferStore::save_buffer`
を呼ぶ。編集・undo・移動などは引き続きZedのkey dispatchへ渡す。dirtyまたは外部delete状態での
`Ctrl-Q` / `Ctrl-W` は初回に警告し、直後の2回目だけ破棄終了・closeにする。

## External file changes

`RealFs`のwatcherから`WorktreeStore`、`BufferStore`、`Buffer::file_updated`までの変更検知は
Zedの既存経路を使う。cleanなBufferの`BufferEvent::ReloadNeeded`だけ、通常のZedでは
`Project`が担う処理をtabのsubscriptionから`BufferStore::reload_buffers`へ渡す。
zecは`Project`を生成せず、この薄いglue以外にreloadや差分適用を再実装しない。

dirtyなBufferは自動reloadせず`has_conflict`をstatusの`!`で示す。競合中の`Ctrl-S`は初回に
警告し、再押下した場合だけdiskを上書きする。`Ctrl-R`はactive fileを明示的にreloadし、dirty
なら同じキーの再押下を要求する。reloadにはhistoryを残すZedのtransactionを使うため、直後の
`Ctrl-Z`でreload前の本文へ戻せる。

reload完了はterminal eventで描画loopを起こし、active tabで検索中ならmatchを再計算する。
完了eventはBuffer IDでtabを引き直すため、処理中にtabが閉じられても古いhandleやlabelを参照しない。
`OpenDocument`はfile path/labelをcacheしない。表示、save/reload可否、error context、終了保護は
その時点の`Buffer::file()`と`DiskState`から導出するため、外部rename後は同じBufferの新pathと
labelへ追従する。外部deleteはclean Bufferでも`!`を表示し、close/quitの破棄確認対象にする。
`Ctrl-S`の再押下はZedの通常save経路で同じpathを再作成する。

## Syntax highlighting

Zedの `grammars` crateが同梱するnative parserを直接 `LanguageRegistry` に登録する。
`native_grammars()` の20組に加え、TSX parserを共有するJavaScriptとRust parserを共有する
Zed Keybind Contextを登録し、Zed本体と同じ22個のbundled language config/queryを扱う。
通常ファイルとして選ばれるのはShell、C/C++、CSS、Diff、Go/Go Mod/Go Work、JSON/JSONC、
JavaScript/TypeScript/TSX、Markdown、Python、Rust、YAML、Git Commitで、残りは主に
injection用のhidden languageである。

`languages::init` はLSP adapterやNode runtimeまで初期化するため、この段階では使わない。
つまり複数言語のtree-sitter解析は行うが、LSPや外部processは起動しない。現在は拡張子で
言語を選ぶため、拡張子のないscriptをshebangだけで判定する処理と、native set外のgrammar、
未登録言語へのinjectionは後続課題とする。

全languageを `Language::new` で起動時に構築すると、使わないqueryまでcompileして初画面が
数秒遅れる。Zed本体と同じくnative grammarを先に登録し、各configは
`LanguageRegistry::register_language` のloaderとして登録する。queryはroot languageまたは
injectionが実際に要求した時だけload/compileする。このcheckoutのdebug buildでは、
Rustファイルの初画面が約5.6秒から約0.6秒、JavaScriptが約5.4秒から約1.0秒になった。

表示時は `DisplaySnapshot::highlighted_chunks` にtree-sitter stylingを要求し、Zedの
themeで解決済みのstyleを行ごとのterminal-cell範囲へ変換する。Ratatuiではbase/syntaxを
描いた後にselectionを重ねる。24-bit color、bold、italic、underline、strikethroughは
端末へ写し、font weightの細分やwavy underlineなど端末にない表現は落とす。

parse完了は非同期なので `BufferEvent::Reparsed` をterminal event channelへ戻して再描画
する。これにより、入力イベントを待たずにhighlightが現れる。

## Terminal Workspace, panels, and recovery

Alpha 3以降はpane、item、dock、panel、overlay、focusを`WorkspaceModel`だけが所有する。layoutは
stable `PaneId`をleafに持つbinary split tree、paneはstable `ItemId`のordered list、dockは
`PanelKind`のordered listである。すべての操作はreducerを通り、layout leafとpane mapの一致、itemの
一意性、有効なactive/focus、preview/pin、overlayの単調IDをtransaction後に検査する。renderer、
mouse hit test、session writerは同じimmutable snapshotを読むため、別々のpane/tab配列を同期しない。

各itemの本文、selection、fold、wrap、undoは引き続きZed `Editor` / `Buffer` / `DisplayMap`がauthorityである。
splitは同じBufferを別Editor viewportへ投影する新しいitemを作るが、Buffer Entityを複製しない。
4-pane、dock、tab strip、statusは`workspace_render`が1回のlayout projectionで非重複Rectへ割り当て、狭幅では
active paneを優先して縮退する。keyboard routeが常にあり、mouse対応時だけsplit/dock境界dragとpanel row
hit targetを追加する。

Project panelはZed Worktree snapshotからstable entry IDを投影し、filter中はancestorだけでなく最初の実matchへ
selectionを移す。create/rename/delete/copyはcanonical path、trust、special-file、symlink/case-fold collisionを
preview時とapply時に再検査し、成功後のscanだけでtreeを更新する。dirty fileのdeleteでもBuffer Entityを残し、
通常のdiscard guardへ接続する。

Project Searchはliteral/regex、case、word、ignored、open-buffer-only、full-path/include/exclude optionを1つの
generation付きcoordinatorで実行する。表示は先頭100 hitにvirtualizeする一方、replace-allは最大10,000 rangeの
`all_matches`を保持する。previewは全sourceのBuffer generationとdisk fingerprintを記録し、accept直前の1件でも
変化していれば部分適用せず全体を中止する。検索結果、references、diagnostics、適用結果はsource BufferとAnchorを
共有するeditable MultiBufferとして描画する。

Outlineとdiagnosticsは一時popupではなくright/bottom dockのpersistent panelである。非同期outline/LSP completionは
source Buffer IDとgenerationを照合してstale resultを捨てる。panel filter、fold、soft wrap、multiple cursor、
inlay hint、inline diagnostic、indent/whitespace guideはZed snapshotから可視cellだけへ投影する。terminal幅は
Unicode graphemeのcell幅で計算し、mouse drag/double/triple clickも同じprojectionを逆変換してZed selection actionへ
戻す。

sessionはrepository identity、workspace、item、dock、navigation、selection、viewport、fold/wrapをversioned JSONへ
保存する。generation fileはimmutableでpayload SHA-256をfilenameとenvelopeの両方に持ち、temporary fileのfsyncと
atomic renameでcommitする。dirty/scratch/MultiBuffer sourceはcontent-addressed blobへ分離し、clean save後のGCも
store全generationを走査して到達可能blobを残す。restoreは新しいgenerationから検証し、truncated/hash/schema不正を
`.rejected-*.session`へ隔離して前世代またはfresh workspaceへfallbackする。同一repositoryの複数processはleaseと
単調generationにより互いのsessionを上書きしない。

Kitty keyboard、modifyOtherKeys、SGR/legacy mouse、focus reporting、OSC 52、OSC 8は起動時に能力検出し、F4 statusへ
routeを表示する。区別不能なchordや未対応mouse操作にはcommand palette/key routeを残し、入力を無言で捨てない。

## Local development services

Git panelはZed `GitStore`のactive repository snapshotをframeごとにimmutable projectionへ変換する。
stage/unstage/discardは選択pathまたはrepository全体のZed APIへ戻し、zec側で`git status`をparseした並行modelは
作らない。terminal panelは`Entity<zed_terminal::Terminal>`を保持し、Zed terminalのcell、cursor、scroll state、
process statusをdockへ描画する。task pickerは`TaskInventory`へactive worktree/bufferの`TaskContexts`を渡し、
resolved `SpawnInTerminal`を`Project::create_terminal_task`で同じterminal collectionへ追加する。

debug configurationも同じTaskInventoryから取得し、登録済み`DapRegistry` adapterだけを表示する。relative path、
Zed task variable、build task、debug locatorを解決した`DebugTaskDefinition`を`DapStore::new_session`と
`boot_session`へ渡す。sessionのthread/frame/scope/variable/outputは`DebugSession`からその都度snapshot化し、
breakpointはProjectの`BreakpointStore`をauthorityにする。DAP reverse `runInTerminal`はZed integrated terminalを
作成してPIDをadapterへ返すため、debuggee processだけを別のshell pathで起動しない。

Zed DapStoreはshutdown eventでsession mapからEntityを除く。error outputが同時に消えるのを防ぐため、zecは最近の
8 Entityだけをpresentation historyとして保持する。これはsession stateの複製ではなく参照寿命の延長であり、
terminated Entityへのcontrol/REPLは拒否する。GDBのようにcontinue eventを抑制するadapterでは、古いglobal stop
flagより具体的なthread statusを優先してpanel stateを投影する。

## Extension ecosystem and distribution

interactive起動ではZed production `Client`、`NodeRuntime`、extension host、language/debug/theme extension bridgeを
初期化し、1つの`ExtensionStore`をregistry、installed state、operation lifecycleのauthorityにする。zecのextension
pickerはstore snapshotのbounded projectionであり、install/upgrade/uninstall/reloadとdevelopment extensionの
install/rebuildを公開APIへ戻す。remote registry失敗時もinstalled recordsを消さず、failureをstatusへ出す。

theme/icon theme pickerは`ThemeRegistry`の現在値と全候補を投影し、選択結果をZed user settings APIで永続化する。
settings/keymap actionはZedが解決した実fileを通常のBufferとして開く。watcherからのsettings/keymap reloadを共有event
loopへ戻すため、編集直後に新しいtheme、editor projection、bindingが反映され、parse failureは旧valid stateを保つ。

standalone updateはstrictなversion 1 JSON manifestから現在OS/architectureのraw executableを選ぶ。remote inputは
HTTPSに限定し、sizeとSHA-256を検証したcandidateだけを一時fileへ同期し、`--version`の完全一致を確認してから同じ
directoryへatomic persistする。明示downloadは既存pathを上書きせず、Unixのself-updateはresolved current regular
executableだけを置換する。Windowsはrunning executableを置換せず、verified download後の明示的な入替を要求する。

release workflowはLinux/Windows/macOSのx86-64/ARM64をnative runnerでbuildし、raw binaryと最小archiveを生成する。
`script/release-manifest`がmetadata、archive member、raw/archive同一性、全checksum、target一意性を検証し、そのfile set
だけをGitHub attestationとreleaseへ渡す。OS signing/notarizationは秘密鍵を必要とする別境界で、資格情報がない状態を
署名済みとして扱わない。normativeな安全条件とgateは[`beta-2.md`](beta-2.md)に置く。

## Rich content and large files

Markdown previewはpinned Zedと同じparse optionを使い、terminal cell向けのbounded snapshotへ変換する。
source Bufferは引き続きZedがauthorityで、previewは毎frameの変更検出で更新する。local linkはactive
worktreeの`ProjectPath`へ解決し、heading fragmentをBuffer位置へ変換して通常tabへ移動する。外部URLは
scheme allowlist、再確認、platform opener capabilityの三段階を通す。

画像はextension判定だけで直接読むのではなく`Project::open_image`へ渡し、Zedが返すImageItemのbytesと
metadataだけをpresentationへ写す。入力byte数、decoded pixel数、protocol outputをそれぞれ制限する。
Kittyはimage IDのdelete、iTerm2/Sixelはalternate-screen clear後の全frame redrawで残像を消す。protocolが
なければ同じtabにformat・寸法・byte数を表示する。画像tabは`SessionItemKind::Image`としてpathだけを永続化し、
実装用scratch Bufferをdirty recoveryとして保存しない。overlayがあるframeではEditor snapshotを描いてから
Rich Contentを抑止し、trust/picker/confirmationを不可視のまま入力だけ奪う状態を作らない。

large-file用の別text modelやtruncateされた保存経路は設けない。通常のZed Editor/Buffer、DisplaySnapshot、
Go-to-line、BufferStore saveを使い、10万行・64 KiB行のactual-binary PTY gateでfirst frame、long-line render、
末尾edit、disk内容、Linux VmHWM、terminal lifecycleを測る。Alpha 1の500-edit benchmarkとBeta 2の境界形状試験は
別oracleとして維持する。

## Buffer search (`Ctrl-F`, active Buffer only)

この節のBuffer内検索は`Alt-F`のrepository-wide Project Searchとは別機能である。
`Ctrl-F`はactive tabのBufferだけを対象にし、repositoryの`Search::local`や
`RepositoryIndex`を使わない。

`Ctrl-F` の本文検索はWorkspaceのGUI search barを生成せず、`Editor` が実装する公開
`SearchableItem` APIを直接使う。`SearchQuery` の実行、matchのstable anchor、active match、
next/previousのwrap、selection、autoscroll、highlight色はZed側に任せる。zecが所有するのは
status行に表示するsingle-line queryとそのcursor、match一覧をAPIへ戻すための短命なsession
だけで、本文やundo stateは持たない。

Zedのbackground highlightを `DisplayPoint` で取得してterminal cell範囲へ変換し、syntaxの
後、selectionの前に背景色だけを重ねる。現在は大文字小文字を区別しないliteral検索を
query変更ごとに逐次awaitする。regex/word/case option、history、長時間検索のcancel/debounce、
multiline queryは後続課題とする。

`Ctrl-H`で同じsessionにsingle-line replacement promptを加える。query/replacementの入力と
focusだけはterminal chromeが持つが、置換範囲、anchor、編集順、transaction、undoは公開
`SearchableItem::replace` / `replace_all`へ戻す。Editor実装は単一置換を1 transaction、全件を
まとめて1 transactionにするため、zecは本文editを組み立てない。単一置換では編集前のmatch
anchorで次へ進んでから検索し直し、replacement自体がqueryを含んでも同じmatchに留まりにくくする。
全置換後も明示的に再検索し、terminal側のmatch countとbackground highlightを同期する。

legacy terminalでは`Ctrl-Enter`と`Enter`を区別できない場合があるため、replace欄では
`Enter`を単一置換、`Alt-Enter`を全置換のportableな操作とし、識別できる場合だけ
`Ctrl-Enter`も全置換として受ける。replacementの改行入力はsingle-line promptの境界外として
後続課題にする。prompt中の本文shortcut漏れを防ぐため、置換の`Ctrl-Z`は`Esc`で検索を
閉じた後に実行する。

## Go to line

`Ctrl-G`のZed actionはWorkspace modalを要求し、standalone Editorではhandlerが何も行わない。
そのため入力欄だけはterminal-ownedな`LinePrompt`にし、確定後の位置解決とselection変更は
Zedの公開APIへ戻す。入力形式はabsoluteな`line[:column]`で、lineとcolumnは1-basedとする。

対象はactive Bufferの`BufferSnapshot::point_from_external_input`で解決する。このAPIを使うことで
Unicode columnをUTF-8 byte columnと取り違えず、範囲外のline/columnもZed本体と同様に文書境界・
行末へclipできる。得たBuffer PointをMultiBuffer Anchorへ変換し、
`Editor::change_selections`と`SelectionEffects::scroll(Autoscroll::center())`で全selectionを1つの
caretへ畳む。本文やundo transactionは変更しない。fold/wrap/display rowをzec側では計算しない。

hidden GPUI Editorのscroll位置はterminal viewportのsource of truthではないため、端末側では既存の
`keep_cursor_visible`が移動先を最小scrollで表示する。Zed GUIと同じ厳密な中央寄せ、相対指定、
入力中のpreview highlightは後続課題とする。

## Terminal scrolling

mouse wheelはactive tabのterminal viewportを3 display rowずつ動かす。
`Alt-PageUp` / `Alt-PageDown`はstatusを除くterminal本文の高さから1行引いた量（最小1行）を使い、
前後の画面を原則1行重ねる。どちらもZedへkeystrokeを送らず、selection、cursor、undo transactionは
変更しない。

manual scroll中はvertical cursor followだけを止める。Zed側のcursor位置が変わった時点で
自動追従へ戻し、horizontal cursor followは常に維持する。単なるRedrawやResizeではmanual
状態を解除せず、viewportが文書末尾を越えた場合だけ有効範囲へclipする。

modifierなし、または`Shift`付きの`PageUp` / `PageDown`は引き続きZedのkeymapへ渡す。
hidden GPUI windowのpage sizeはterminal本文の高さと一致しないため、この経路の移動量は既知の
境界とする。またmouse capture中にterminal自身の文字選択を使う場合、多くのterminalでは
`Shift`付きdragが必要になる。

## Terminal mouse positioning

modifierなしの左button Downだけをcaret移動として扱う。Ratatuiで描画したのと同じ
grapheme/cell幅、ガター、viewportを使ってscreen cellをdisplay行のUTF-8 byte位置へ
逆変換する。wide graphemeはcellの中心に最も近い境界へ寄せ、viewport境界で
切れて描画されないgraphemeの空白cellはclick不可とする。

逆変換の結果はclick処理時点の最新`DisplaySnapshot`でclipし、
`display_point_to_anchor -> Editor::change_selections`へ渡す。zecはcaretやselection状態を所有せず、
mouse clickでBuffer本文やundo transactionも変更しない。ガター、status行、本文外、
right/middle button、modifier付きclickはcaretやBufferに作用しない。promptや検索の入力中も
本文のcaretは動かさない。

Zed GUIのdrag、word/line selection、multi-cursorのmouse state machine入口は現在
`pub(super)`である。そのロジックをzecへ複製せず、drag/double/triple/modifier selectionは
Zed側に小さな公開hookを追加するか判断するまで後続課題とする。

## Tabs

1 tabにつき `Editor::for_buffer` をrootにした非表示GPUI windowを1つ持つ。Zedのfocus、
key context、action dispatch、selection、cursor、undo、DisplayMapはwindowごとそのまま使い、
zecが持つ可変状態はactive indexとterminal viewport、そのfollow状態だけにする。公開 `replace_root` では既存の
Editor Entityをrootへ付け替えられず、1 window内でchildを交換すると専用hostとfocus treeの
同期が必要になるため採用しない。

`Ctrl-PageUp` / `Ctrl-PageDown` はWorkspace/Paneを作っていないのでzecが捕捉し、activeな
WindowHandleだけを描画・入力対象にする。各windowのfocusはwindow-localなのでOS windowの
activateは不要。切替時はBuffer固有Anchorを別Editorへ渡さないよう検索をcloseし、Save As
とOpen promptもcancelする。statusはactive位置と全tabのdirty状態を表示し、`Ctrl-S` は
activeだけ、`Ctrl-Q` は全Bufferを検査する。

`Ctrl-O`はterminal-ownedなsingle-line promptからpathを絶対化し、起動時と同じ
`open_document`へ渡す。共有BufferStoreが同じBuffer Entityを返した場合は既存tabへ移動し、
新しいBufferの場合だけhidden windowとsyntax再描画subscriptionを追加する。open失敗時は
promptへerrorを返し、既存tabsとactive indexは変更しない。

`Ctrl-N`は同じBufferStoreの`create_local_buffer`を使う`open_document(None)`経路でscratchを
作り、`Untitled N`というprocess内で単調増加する表示名を付ける。裸のBufferを作らないため、
後のSave Asでfile path mappingと外部変更監視へ正常に移行する。新しいscratchも通常tabと同じ
hidden window、dirty保護、Save As、close lifecycleを使う。active tabが変わる操作なので、
検索を閉じ、入力途中のSave As/Open promptはtab切替と同様にcancelする。

`Ctrl-W`はclean tabを即座に閉じ、dirty tabでは同じキーの再入力を要求する。他のkey pressや
pasteで確認状態を解除する。GPUIのWindowHandleはwindowを所有しないため、handleをdropするだけ
ではなく`Window::remove_window`を呼んだ後にDocumentTabを除去する。最後のwindowを閉じた場合は
GPUIのheadless runtimeも終了する。BufferStoreはweak参照を保持するため、破棄したdirty tabを
同じpathで開き直すとdiskから新しいBufferがloadされる。

## Why not `ratatui-textarea`

`ratatui-textarea` は表示だけでなく、テキスト、カーソル、selection、入力処理、undo
履歴を所有する。そのためメイン編集領域に使うと Zed と編集状態が二重化し、undo、
keymap、複数 selection、fold/inlay の同期が必要になる。

メイン編集領域には状態を持たない専用 Ratatui Widget を使い、
`DisplaySnapshot -> terminal cells` の変換だけを実装する。
検索・置換欄、Save As、Open、Go to lineはZed管理外の小さなsingle-line入力なので、
共通の軽量prompt stateをzecが
持つ。必要な操作が文字入力、cursor移動、削除、submit、cancelだけであるため、現時点では
`ratatui-textarea`を追加せず、この境界を約1型に限定している。

## Boundaries

- Zed の表示 column は UTF-8 byte 基準、端末は grapheme/cell 幅基準なので、zec に
  一箇所だけ座標変換層を置く。mouse hit testも同じcell metricsを使う逆変換にする。
- Zedの全selectionを表示座標の半開区間として取得し、端末cell座標へ変換してから
  Ratatuiの文字描画後に反転styleだけを重ねる。文字列やselection状態は複製しない。
- 行番号はdisplay rowを数えず、`DisplaySnapshot::row_infos` の `buffer_row` を表示する。
  block rowやsoft-wrap継続行は空欄にし、`widest_line_number` からガター幅を固定する。
  本文、cursor、selection、横scrollはすべてガターを除いた同じRectで計算する。
- 初期段階では soft wrap を無効にして横スクロールを使う。GPUI の pixel 幅と端末の
  cell 幅を混ぜない。
- terminal reader は別 thread で blocking input を読み、channel 経由で GPUI
  foreground に渡す。Editor 自体は単一 thread で操作する。resize event が欠ける
  PTY向けに、同じreader threadで低頻度のsize確認も行う。
- headless clipboard は使えないため、bracketed pasteの文字列をZedの `do_paste` へ
  直接渡す。これによりpaste時のselection置換、auto-indent、undo単位はZedに任せる。
- GPUIのLinux headless clipboardはwriteがno-op、readが常に空で、ZedのCopy/Cutが作る
  `ClipboardItem` を返す公開APIもない。そのためcopy payloadに限り、Zedの公開selectionと
  buffer snapshotから本体と同じ行・multi-selection規則で組み立てる薄いadapterを置く。
  Editor、selection、編集transaction、undo stateは所有しない。CutはpayloadをOSC 52へ
  書けた後にZedのCut actionをdispatchし、削除とundoをZedへ任せる。
- OSC 52はtextだけを運び成功応答を持たない。Zedのclipboard metadataをterminal越しに
  保持できないため、bracketed pasteには推測したmetadataを付けず常に外部textとして渡す。
  端末ごとのcontrol-string上限を踏みにくくするためraw textを256 KiBに制限し、超過時は
  Copyを拒否し、Cutなら本文も変更しない。

## Deferred

tabのreorder UI、LSP、検索option/history、Go to lineの相対指定/live preview、clipboard metadata/read、native set外の
grammar、完全なlanguage injection、terminal-aware soft wrap、mouse drag/double/triple/modifier selection、
外部rename/deleteの詳細なrecovery UIは後続で追加する。

自動確認には `--smoke` と単体テストを使う。端末経路はPTY上で文字入力、undo、
新規・既存ファイルの保存、scratchのsave-asと上書き確認、OSC 52 copy、Zed Cutのundo、
tabごとの編集・undo・active save・全tab dirty終了保護、実行中のopen・dedupe・未作成path保存・
scratch tab追加とSave As、cleanな外部変更の自動reload、dirtyな外部変更のreload/overwrite確認、
dirty close後のdisk reload・最後のwindow終了、ASCII/wide文字上のmouse clickによるcaret移動、
raw mode / alternate screenの復元まで確認する。
