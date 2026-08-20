# zec

Zed の編集コアを使う CUI エディタの実現可能性を検証するプロジェクトです。

現在は、GPUI の headless runtime 上で `editor::Editor` を動かし、Crossterm から
Zed の keymap へ入力を渡し、Ratatui で plain text とカーソルを描画します。

設計判断と今後の構成は [docs/architecture.md](docs/architecture.md) に記録します。

```sh
cargo run
```

空のbufferで起動します。文字入力、移動、`Ctrl-Z` のundo、`Ctrl-Y` のredoがZedの
編集処理を通ります。終了は `Ctrl-Q` です。

端末を使わず、最初の挿入・undo PoCだけを実行する場合:

```sh
cargo run -- --smoke
```

初回はZedの依存一式をビルドするため、時間とディスク容量を使います。
