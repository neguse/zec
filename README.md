# zec

Zed の編集コアを使う CUI エディタです。Linux向け基盤の実現可能性PoCは、
data/terminal lifecycle、viewport-bounded rendering、actual-binary PTY試験、
clean-build再現性の全gateを2026-08-23に通過しました。production-readyではなく、
ここからalpha editorとして開発します。

現在は、GPUI の headless runtime 上で `editor::Editor` を動かし、Crossterm から
Zed の keymap へ入力を渡し、Ratatui で本文、カーソル、selection、syntax styleを
描画します。

設計判断と今後の構成は [docs/architecture.md](docs/architecture.md) に、
PoCの卒業判定、検証記録、既知制約は [docs/poc-graduation.md](docs/poc-graduation.md) に記録します。

```sh
cargo run -- path/to/file another/file
```

既存ファイルと未作成ファイルのどちらも複数指定でき、実行中も`Ctrl-O`から追加できます。
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

引数なし、または`Ctrl-N`では空のscratch bufferを開きます。`Ctrl-S`で1行のSave As promptに入り、
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
parser/queryを遅延loadします。現在は実行時にLSPやNode runtimeを初期化しません。
`NO_COLOR` が設定された環境ではCrosstermの規約どおり色を出しません。

端末を使わず、Zed Editorへの挿入とundoを確認するheadless smoke:

```sh
cargo run -- --smoke
```

初回はZedの依存一式をビルドするため、時間とディスク容量を使います。
