# zec

Zed の編集コアを使う CUI エディタです。repository/language editingとTerminal Workspace、
Git/terminal/tasks/DAPのlocal development loopに加え、Zed extension host、theme、settings/keymap、
manifest検証付きupdate、6-target release pipeline、Zed remote protocolによるSSH workspace、
Markdown/画像、10万行large-file path、Zed Agent/ACP/MCP、edit prediction/inline assistant、
channels/channel notes/following、voice/screen bridge、native Zed Notebookまでimplemented candidateです。
機械可読台帳の27 capabilityすべてに実装証跡があります。Alpha 3の361-case actual-binary acceptanceと
860-ID benchmarkはlocalで完走していますが、default-branch hosted evidence、live service、
全platformの証跡待ちなので、まだverifiedでもproduction-readyでもありません。

現在は、GPUI runtime 上で `editor::Editor` を動かし、Crossterm から Zed の
keymap へ入力を渡し、Ratatui で本文、カーソル、selection、syntax styleを描画します。
Linuxではheadless platformを、Windowsでは非表示windowを作るnative platformを使います。

設計判断と今後の構成は [docs/architecture.md](docs/architecture.md) に、
PoCの卒業判定、検証記録、既知制約は [docs/poc-graduation.md](docs/poc-graduation.md) に記録します。
長期のZed体験parityは [docs/zed-parity.md](docs/zed-parity.md) と
[docs/zed-parity-v1.json](docs/zed-parity-v1.json)で追跡します。repository editing loopは
[docs/alpha-1.md](docs/alpha-1.md)、次のProject-backed language editing loopは
[docs/alpha-2.md](docs/alpha-2.md)、Terminal Workspaceは[docs/alpha-3.md](docs/alpha-3.md)、
local development loopは[docs/beta-1.md](docs/beta-1.md)、ecosystemと配布は
[docs/beta-2.md](docs/beta-2.md)、AI/collaboration/mediaは
[docs/parity-1.md](docs/parity-1.md)の機械判定contractで管理します。

Alpha 2の通常回帰は、unit/PTYに加えて次のactual-binary integration testで確認できます。
canonicalな20-process acceptanceとbenchmark、Alpha 1再検証、evidence-only promotionを含む完全な
判定手順は[docs/alpha-2.md](docs/alpha-2.md)を参照してください。

```sh
cargo test --locked --test parity_contract --test pty_acceptance \
  --test alpha_2_lsp --test alpha_2_settings --test alpha_2_failures \
  -- --test-threads=1
```

引数なしなら現在のdirectory、directoryを1つ渡した場合はそのdirectoryをrepository rootとして
起動します。開発checkoutから実行する場合は、それぞれ`cargo run --`、
`cargo run -- path/to/repository`と同じです。

```sh
zec
zec DIRECTORY
```

repository modeでは`Ctrl-P`でQuick Openを開き、root配下のfileを絞り込んで`Enter`で開けます。
`Alt-F`はrepository全体の大文字小文字を区別するliteral検索です。結果はpath、1-basedのlineと
Unicode column、previewとして表示し、先頭100件までを上下矢印で選んで`Enter`でそのmatchへ
移動できます。Unicode normalization、regex、project-wide replaceは行いません。

Quick OpenまたはProject Searchは`Esc`でcancelできます。Project Search中にqueryを変更した場合は
最新queryの結果だけを表示し、順序が前後して完了した古いqueryの結果は描画も適用もしません。
`Esc`で閉じた後や別操作へ移った後に完了した結果も無視します。

従来のdirect file modeも残しています。先頭引数がfileなら、既存ファイルと未作成ファイルの
どちらも複数指定でき、実行中も`Ctrl-O`から追加できます。このmodeではrepositoryを持たないため、
`Ctrl-P`と`Alt-F`は利用できません。

```sh
zec path/to/file another/file
```

`Ctrl-N`では、`Untitled N`という保存先未定のscratch tabを追加できます。
同じBufferがすでに開かれていればtabを重複させず、既存tabへ移動します。
`Ctrl-PageUp` / `Ctrl-PageDown` でtabを切り替え、`Ctrl-W`でactive tabを閉じます。
未保存tabまたはdisk上で削除されたtabは、同じ`Ctrl-W`をもう一度押した場合だけ破棄し、
最後のtabを閉じると終了します。
各tabの本文、cursor、selection、undoは独立したZed Editorが保持し、viewportだけを端末側で
保持します。`Ctrl-S` はactive tabだけをZedの `BufferStore` 経由で保存します。
終了は `Ctrl-Q` です。未保存または削除されたtabが1つでもあれば、破棄確認として
もう一度`Ctrl-Q`を押します。statusには全tabのdirtyを`+`、外部変更との競合または
disk上の削除を`!`で表示します。

cleanなfileがdisk上で変更されると自動でreloadします。local edit中は本文を上書きせず`!`を
表示し、`Ctrl-S`は再押下した場合だけdiskを上書きします。`Ctrl-R`はactive fileをdiskから
reloadし、dirtyなら同じキーの再押下を要求します。reloadもZedのtransactionとして扱うため、
直後の`Ctrl-Z`でreload前の本文へ戻せます。
外部renameは同じZed Bufferのfile identityへ追従し、tab labelと以後の保存先も新pathになります。
外部delete後も本文を保持して`!`を表示し、`Ctrl-S`の再押下で同じpathへ再作成できます。
catch可能な`SIGTERM` / `SIGHUP`は通常の終了経路へ渡し、端末modeを復元してから終了します。

`Ctrl-N`では空のscratch bufferを開きます。`Ctrl-S`で1行のSave As promptに入り、
相対パスはzecを起動したworking directory基準で保存します。親directoryは必要なら作成し、
既存の通常ファイルはもう一度 `Enter` を押した場合だけ上書きします。`Esc` でcancelできます。
Save Asと`Ctrl-O`のpath promptはshellを通らないため、`~` はhome directoryへ展開しません。

`Shift` + 矢印や `Ctrl-A` のselectionもZedのkeymapで動き、選択範囲を端末上に
反転表示します。左ガターの行番号もZedのdisplay snapshotから取得するため、foldなどを
追加した後も表示行を単純に数え直しません。

mouse wheelは3 display rowずつ、`Alt-PageUp` / `Alt-PageDown`は原則terminal本文の高さから
1行引いた量（本文が1行だけなら1行）ずつ、active tabの表示だけをscrollします。Zedのcursor、selection、undoは
変更せず、cursorが移動すれば自動追従へ戻ります。mouse capture中にterminal自身の文字選択を
使う場合、多くのterminalでは`Shift`を押しながらdragします。

modifierなしの左clickで、表示中の本文位置へZedのcaretを移動できます。ガター、
status行、viewport境界で半分に切れたwide文字はclick対象にしません。dragによる
Zed selection、double/triple click、modifier付きmouse操作はまだ対象外です。

`Ctrl-C` はselection（空なら現在行）をterminal clipboardへcopyし、`Ctrl-X` はcopyに
成功してからZedのCut actionで削除します。端末との受け渡しはOSC 52なので、対応端末の
設定やtmuxのclipboard設定が必要な場合があります。端末から成功応答は返らないため、
未対応端末では操作が無視されます。巨大なcontrol sequenceを避けるため1回256 KiBまでです。
pasteは従来どおりterminalのbracketed pasteをZedへ渡します。

`Ctrl-F` で大文字小文字を区別しないliteral検索を開始します。入力中にmatchを更新し、
`Enter` または下矢印で次、上矢印（対応端末では `Shift-Enter` も可）で前へ移動し、
`Esc` で検索を閉じます。match、移動、selection、autoscrollはZedの検索実装を使います。
`Ctrl-H`ではreplace欄も表示します。`Tab` / `BackTab`でqueryとreplace欄を移動し、replace欄の
`Enter`で現在のmatchを1件、`Alt-Enter`（識別できる端末では`Ctrl-Enter`も可）で全件を
置換します。`Esc`で検索を閉じた後、単一置換と全置換はどちらも`Ctrl-Z`で戻せます。

`Ctrl-G`では`line[:column]`形式の1-based位置へ移動します。行やcolumnが範囲外ならZedの
BufferSnapshotで文書境界・行末へclipし、Unicode columnはbyte数ではなく文字位置として解決します。
空入力や数値でない入力はprompt内にerrorを表示し、修正して再実行できます。

Zed同梱のnative tree-sitter parser/config/queryを使い、Shell、C/C++、CSS、Diff、
Go、JSON、JavaScript/TypeScript、Markdown、Python、Rust、YAMLなどをsyntax highlight
します。起動時にはconfigとmatcherだけを登録し、対象ファイルとinjectionに必要な
parser/queryを遅延loadします。repository modeでは同じZed `Project`がLanguageRegistryと
LspStoreも所有し、各language adapterの通常のPATH discoveryでlanguage serverを起動します。
Node runtimeはZedの設定に従ってconfigured/system pathを探索し、必要ならdownloadできる状態で初期化します。
`NO_COLOR` が設定された環境ではCrosstermの規約どおり色を出しません。

`F1`または`Ctrl-Shift-P`はaction ID、表示名、binding、現在のenabled stateを持つcommand
paletteです。`Ctrl-Space`（または`Alt-/`）で補完、`F2`でhover、`F8`でproject diagnostics、
`F12` / `Alt-F12` / `Shift-F12`でdefinition / type definition / references、`Ctrl-T`でproject
symbolsを開きます。移動後は`Alt-Left` / `Alt-Right`でselectionとviewportを含む履歴を往復できます。
`F6`はprepareRename後にrename promptを開き、`Ctrl-.`はcode action picker、`Shift-Alt-F`は文書、
`Ctrl-Alt-F`はselectionをformatします。複数fileに及ぶrename/code actionはZedの
`ProjectTransaction`として保持し、`Ctrl-Z` / `Ctrl-Y`で全対象Bufferを一括undo/redoします。

`Ctrl-Shift-G`はZedのactive repository snapshotをGit panelへ投影します。panel内では個別または
全changeのstage/unstage、discardをZed GitStoreへ適用できます。`Ctrl-\``はbottom dockの
integrated terminal、`Ctrl-Shift-\``は新しいZed terminalを開きます。terminal focus中のkey、paste、
resize、scrollbackはZed terminalへ渡し、`Esc`でeditorへ戻ります。

`Ctrl-Shift-B`はZed TaskInventoryが解決したtask picker、`Ctrl-Alt-B`は完了済みの最後のtaskの
rerunです。taskはintegrated terminalで実行され、同時実行禁止、reveal、save、cwd、environmentなど
Zed taskの設定を保持します。untrusted worktreeではprocessを開始しません。

`F5`は`.zed/debug.json`とtask由来のdebug scenarioを選び、登録済みZed DAP adapterを起動します。
`Ctrl-Shift-D`でDebugger panel、`Ctrl-F9`でsource breakpoint、`Ctrl-F5` / `Ctrl-F6` /
`Shift-F5`でcontinue / pause / stop、`Alt-F10` / `Alt-F11` / `Alt-Shift-F11`でstep over / in / outを
操作します。`Ctrl-Shift-R`（panel focus中は`:`）のdebug consoleはadapter-nativeなREPL commandを
評価します。終了済みsessionは操作対象から外れますが、最後のadapter outputはpost-mortem用に残ります。
詳細と実GDB受け入れ試験は[docs/beta-1.md](docs/beta-1.md)を参照してください。

`Ctrl-Shift-X`はZed ExtensionStoreのall/installed/updates viewです。`Enter`でinstall/update、
`Delete`を2回でuninstall、`Ctrl-D`で`extension.toml`を持つdevelopment extensionを追加し、
追加後の`Enter`でsourceからrebuildします。`Ctrl-Alt-T` / `Ctrl-Alt-I`はextension由来を含む
theme/icon theme、`Ctrl-,` / `Ctrl-Alt-,`は実際のZed settings/keymap fileを開きます。
`Ctrl-Alt-U`はupdate manifestを確認します。CLIによる検証・download・適用とrelease形式の詳細は
[docs/beta-2.md](docs/beta-2.md)を参照してください。

Markdown tabでは`Ctrl-Shift-V`でZed互換のfeature flagを使うpreviewを開き、`Tab`でlink/imageを
選び`Enter`で開きます。local linkはZed ProjectPath内だけに制限し、外部URLは確認後にOSへ渡します。
PNG/JPEG/GIF/WebP/BMP/TIFF/ICO/PNM系画像はQuick Open、Project Panel、`Ctrl-O`、または
`zec image.png`でread-only tabとして開けます。Kitty/iTerm2/Sixelを能力検出し、未対応端末では
format・寸法・sizeを表示します。Markdown/画像tabよりtrustやcommand overlayが常に前面です。

10万行と64 KiB単一行を含むactual-binary gateで、open、`Ctrl-G`移動、描画、編集、保存、Linux
`VmHWM ≤ 1 GiB`、terminal復元を検証しています。これは専用の簡略text modelではなく通常のZed
Editor/Buffer経路です。

`zec remote ssh`はZedと同じremote protocol/serverを使ってremote Projectを開きます。editor、
BufferStore、WorktreeStore、LSP、Git、terminal、tasks、DAP、project search、project panelのauthorityは
接続先のZed Projectに残ります。passwordは引数に受け付けず、必要な認証は端末内のmasked askpassで
行います。WSLとDocker/Podman transportも同じ入口から選べます。

```sh
zec --version
zec remote ssh HOST /absolute/project
zec remote wsl DISTRO /absolute/project
zec remote container NAME /absolute/project
zec update check
zec update download --output ./zec-new
zec update apply
```

## Agent, collaboration, and Notebook

`Ctrl-Shift-A`（またはcommand paletteの`Toggle Agent Panel`）でAgent panelを開きます。
既定はprocess内のnative Zed Agentで、`ZEC_ACP_AGENT`にstrict JSONのcommand/args/env/idを設定した
場合だけexternal stdio ACP agentへ接続します。prompt、stream、tool call、permissionのallow/reject、
cancel、新規sessionに加え、`/models`、`/modes`、`/config`、`/sessions`、`/skills`、
`/instructions`、`/mcp`、`/auth`を操作できます。外部agentを含むprocess開始はworktree trust後です。

`Alt-\`でZed edit predictionを表示し、`Alt-L` / `Alt-K` / `Alt-J`で全体／次word／次lineを
Zed transactionとして受け入れます。providerはZed language settingsの
Zed/Copilot/Codestral/Ollama/OpenAI-compatible設定に追従します。`Ctrl-Enter`のinline assistantは
stream結果をdiff previewにし、`Enter`でaccept、`Esc`でrejectします。

`Ctrl-Alt-C`でZed Client/UserStore/ChannelStoreを使うCollaboration panelを開きます。
矢印と`Enter`でchannel notes、`Tab`と`f`でcollaborator follow、`c`でchannel作成、
`a` / `d`でinvite応答、`i`でsign-in/outを操作します。notesはZed `ChannelBuffer`なのでsplit間で
同じ共同編集authorityを共有します。voice/screenは端末内に偽装せずexternal bridgeです。
`ZEC_MEDIA_BRIDGE`と`ZEC_EXTERNAL_MEDIA=1`の両方がある場合のみ、`v` / `s`の後に毎回`y`で
許可して開始し、stop/終了時にowned processを回収します。

`.ipynb`はZed `NotebookItem` / `NotebookEditor`で開きます。矢印でcell選択、`Enter`で編集、
`Ctrl-Enter` / `Shift-Enter`でrun/run-and-advance、`b` / `m`でcode/Markdown追加、`dd`で削除、
`Alt-Up/Down`で移動、`R`でrun all、`c`でoutput消去、`i` / `r`でkernel interrupt/restartです。
stream/error/Markdownと既存rich outputのmetadata fallbackを投影し、nbformat JSONを通常の
Project Bufferへ保存します。splitは1つのNotebook authorityを共有し、再起動中や最終closeでも
local Jupyter process groupを残しません。詳細・制約・実バイナリ証跡は
[docs/parity-1.md](docs/parity-1.md)を参照してください。

端末を使わず、Zed Editorへの挿入とundoを確認するheadless smoke:

```sh
cargo run -- --smoke
```

初回はZedの依存一式をビルドするため、時間とディスク容量を使います。

## Windows

WindowsではMSVCのC++ build toolsとWindows SDKが必要です。Windows Terminalなどの
ConPTY対応端末からPowerShellを開き、次のようにbuild、smoke、起動を行います。
Zed依存の初回buildは大きいため、空き容量が限られる環境ではdebug infoとincremental buildを
無効にしてください。

```powershell
$env:CARGO_INCREMENTAL = "0"
$env:CARGO_PROFILE_DEV_DEBUG = "0"
$env:CARGO_PROFILE_DEV_BUILD_OVERRIDE_DEBUG = "0"

cargo build --locked --bin zec
.\target\debug\zec.exe --smoke
.\target\debug\zec.exe .
```

通常の編集、repository mode、保存、検索などはWindowsでも利用できます。
POSIX signal/job controlとLinux PTY acceptance、Alpha 1の機械判定gateは引き続きLinux専用です。
Linux専用のAlpha 1補助binaryをbuildする場合は`--features alpha-1-linux`が必要です。
