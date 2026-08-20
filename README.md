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

`Shift` + 矢印や `Ctrl-A` のselectionもZedのkeymapで動き、選択範囲を端末上に
反転表示します。左ガターの行番号もZedのdisplay snapshotから取得するため、foldなどを
追加した後も表示行を単純に数え直しません。

Zed同梱のnative tree-sitter parser/config/queryを使い、Shell、C/C++、CSS、Diff、
Go、JSON、JavaScript/TypeScript、Markdown、Python、Rust、YAMLなどをsyntax highlight
します。現在は実行時にLSPやNode runtimeを初期化しません。`NO_COLOR` が設定された
環境ではCrosstermの規約どおり色を出しません。

引数なしでは空のscratch bufferを開きます。現時点では保存先を選ぶUIがないため、
scratch bufferの `Ctrl-S` は保存せずstatusにエラーを表示します。

端末を使わず、最初の挿入・undo PoCだけを実行する場合:

```sh
cargo run -- --smoke
```

初回はZedの依存一式をビルドするため、時間とディスク容量を使います。
