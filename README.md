# zec

Zed の編集コアを使う CUI エディタの実現可能性を検証するプロジェクトです。

現在は、GPUI の headless runtime 上で `editor::Editor` を動かし、Crossterm から
Zed の keymap へ入力を渡し、Ratatui で本文、カーソル、selection、syntax styleを
描画します。

設計判断と今後の構成は [docs/architecture.md](docs/architecture.md) に記録します。

```sh
cargo run -- path/to/file
```

既存ファイルと未作成ファイルのどちらも開けます。文字入力、移動、`Ctrl-Z` のundo、
`Ctrl-Y` のredoがZedの編集処理を通り、`Ctrl-S` でZedの `BufferStore` 経由で保存します。
終了は `Ctrl-Q` です。未保存の変更がある場合だけ、破棄確認としてもう一度
`Ctrl-Q` を押します。

引数なしでは空のscratch bufferを開きます。`Ctrl-S` で1行のSave As promptに入り、
相対パスはzecを起動したworking directory基準で保存します。親directoryは必要なら作成し、
既存の通常ファイルはもう一度 `Enter` を押した場合だけ上書きします。`Esc` でcancelできます。
promptはshellを通らないため、`~` はhome directoryへ展開しません。

`Shift` + 矢印や `Ctrl-A` のselectionもZedのkeymapで動き、選択範囲を端末上に
反転表示します。左ガターの行番号もZedのdisplay snapshotから取得するため、foldなどを
追加した後も表示行を単純に数え直しません。

`Ctrl-C` はselection（空なら現在行）をterminal clipboardへcopyし、`Ctrl-X` はcopyに
成功してからZedのCut actionで削除します。端末との受け渡しはOSC 52なので、対応端末の
設定やtmuxのclipboard設定が必要な場合があります。端末から成功応答は返らないため、
未対応端末では操作が無視されます。巨大なcontrol sequenceを避けるため1回256 KiBまでです。
pasteは従来どおりterminalのbracketed pasteをZedへ渡します。

`Ctrl-F` で大文字小文字を区別しないliteral検索を開始します。入力中にmatchを更新し、
`Enter` または下矢印で次、上矢印（対応端末では `Shift-Enter` も可）で前へ移動し、
`Esc` で検索を閉じます。match、移動、selection、autoscrollはZedの検索実装を使います。

Zed同梱のnative tree-sitter parser/config/queryを使い、Shell、C/C++、CSS、Diff、
Go、JSON、JavaScript/TypeScript、Markdown、Python、Rust、YAMLなどをsyntax highlight
します。起動時にはconfigとmatcherだけを登録し、対象ファイルとinjectionに必要な
parser/queryを遅延loadします。現在は実行時にLSPやNode runtimeを初期化しません。
`NO_COLOR` が設定された環境ではCrosstermの規約どおり色を出しません。

端末を使わず、最初の挿入・undo PoCだけを実行する場合:

```sh
cargo run -- --smoke
```

初回はZedの依存一式をビルドするため、時間とディスク容量を使います。
