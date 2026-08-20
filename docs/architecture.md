# Architecture decisions

Status: Accepted (2026-08-20)

## Goal

Zed の `editor::Editor` を編集機能の本体として使い、端末固有の入出力だけを追加する。
Editor、Buffer、selection、undo、keymap は再実装しない。

headless の挿入・undo PoCに加え、plain textの端末表示、キー入力、移動、undo/redo、
paste、resize、終了時の端末復元まで実装済み。

## Repository strategy

Zed monorepoの fork ではなく、独立した binary crate から Zed の各 crate を Git
dependency として使う。検証対象が勝手に変わらないよう revision は `Cargo.toml` で
固定する。Zed 側の private API が本当に必要になるまでは fork や本体変更を持たない。

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
```

| Component | Responsibility |
| --- | --- |
| Zed Editor / Buffer | テキスト、カーソル、selection、編集 action、undo |
| GPUI headless | Zed の runtime と window/action context |
| Crossterm | raw mode、キー・paste・resize入力、端末への出力 |
| Ratatui | レイアウト、cell buffer、style、差分描画 |
| zec | 端末イベント変換、Zed snapshot の cell 化、処理の接続 |

編集状態の source of truth は常に Zed とする。端末入力は Zed の action/input 経路へ
渡し、描画側は Zed の immutable な表示 snapshot を読むだけにする。

Ratatui は cell buffer、style、レイアウト、前後 frame の差分描画を既に提供するため
採用する。入力 loop は所有しないので、Crossterm で読んだイベントを zec が Zed の
入力へ変換する。

## Why not `ratatui-textarea`

`ratatui-textarea` は表示だけでなく、テキスト、カーソル、selection、入力処理、undo
履歴を所有する。そのためメイン編集領域に使うと Zed と編集状態が二重化し、undo、
keymap、複数 selection、fold/inlay の同期が必要になる。

メイン編集領域には状態を持たない専用 Ratatui Widget を使い、
`DisplaySnapshot -> terminal cells` の変換だけを実装する。
`ratatui-textarea` は、必要なら検索欄など Zed 管理外の小さな入力欄に限定して使う。

## Boundaries

- Zed の表示 column は UTF-8 byte 基準、端末は grapheme/cell 幅基準なので、zec に
  一箇所だけ座標変換層を置く。
- 初期段階では soft wrap を無効にして横スクロールを使う。GPUI の pixel 幅と端末の
  cell 幅を混ぜない。
- terminal reader は別 thread で blocking input を読み、channel 経由で GPUI
  foreground に渡す。Editor 自体は単一 thread で操作する。resize event が欠ける
  PTY向けに、同じreader threadで低頻度のsize確認も行う。
- headless clipboard は使えないため、paste は bracketed paste を直接渡す。copy の
  terminal bridge は後続課題とする。

## Deferred

現在は最新cursorのみ描画し、selection範囲のstyleはまだ描画しない。
syntax highlight、file I/O、tabs、LSP、terminal-aware soft wrapは後続で追加する。

自動確認には `--smoke` と単体テストを使う。端末経路はPTY上で文字入力、undo、終了と
raw mode / alternate screenの復元まで確認する。
