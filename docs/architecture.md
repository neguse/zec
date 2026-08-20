# Architecture decisions

Status: Accepted (2026-08-21)

## Goal

Zed の `editor::Editor` を編集機能の本体として使い、端末固有の入出力だけを追加する。
Editor、Buffer、selection、undo、keymap は再実装しない。

headless の挿入・undo PoCに加え、plain textの端末表示、キー入力、移動、undo/redo、
selection表示、論理行番号、native languageのsyntax highlight、paste、resize、
実ファイルのopen/save、dirty表示、終了時の端末復元まで実装済み。

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

FILE
    -> Zed RealFs
    -> WorktreeStore
    -> BufferStore
    -> Zed Buffer
    -> tree-sitter parse / highlighted chunks
```

| Component | Responsibility |
| --- | --- |
| Zed Editor / Buffer | テキスト、カーソル、selection、編集 action、undo、dirty状態 |
| Zed BufferStore / WorktreeStore / RealFs | ファイルのopen/save、encoding・改行・disk state |
| Zed Language / tree-sitter | language query、parse、syntax highlight range |
| GPUI headless | Zed の runtime と window/action context |
| Crossterm | raw mode、キー・paste・resize入力、端末への出力 |
| Ratatui | レイアウト、cell buffer、style、差分描画 |
| zec | 端末イベント変換、Zed snapshot の cell 化、処理の接続 |

編集状態の source of truth は常に Zed とする。端末入力は Zed の action/input 経路へ
渡し、描画側は Zed の immutable な表示 snapshot を読むだけにする。

Ratatui は cell buffer、style、レイアウト、前後 frame の差分描画を既に提供するため
採用する。入力 loop は所有しないので、Crossterm で読んだイベントを zec が Zed の
入力へ変換する。

## File I/O

`zec FILE` はCLI入力を絶対パス化し、Zedの `RealFs -> WorktreeStore -> BufferStore` で
開く。`Project` 全体や手書きの `std::fs::write` は使わない。これによりencoding、BOM、
改行コード、保存version、外部ファイル状態をZed側の実装に任せられる。

該当worktreeがなければ、ファイル自身を非表示のsingle-file worktreeとして扱う。
未作成パスもfile付きの `DiskState::New` Bufferになるため、編集後の `save_buffer` で
新規作成できる。引数なしのscratchはfileを持たないので、save-as UIを実装するまでは
保存不可とする。

Zed既定keymapの `Ctrl-S` はWorkspace actionだが、このbinaryのrootはEditorなので
保存handlerがない。そのため `Ctrl-S` だけCLI側で捕捉して `BufferStore::save_buffer`
を呼ぶ。編集・undo・移動などは引き続きZedのkey dispatchへ渡す。dirtyな状態での
`Ctrl-Q` は初回に警告し、直後の2回目だけ破棄終了にする。

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

表示時は `DisplaySnapshot::highlighted_chunks` にtree-sitter stylingを要求し、Zedの
themeで解決済みのstyleを行ごとのterminal-cell範囲へ変換する。Ratatuiではbase/syntaxを
描いた後にselectionを重ねる。24-bit color、bold、italic、underline、strikethroughは
端末へ写し、font weightの細分やwavy underlineなど端末にない表現は落とす。

parse完了は非同期なので `BufferEvent::Reparsed` をterminal event channelへ戻して再描画
する。これにより、入力イベントを待たずにhighlightが現れる。

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
  copy のterminal bridgeは後続課題とする。

## Deferred

save-as、tabs、LSP、native set外のgrammar、完全なlanguage injection、terminal-aware
soft wrapは後続で追加する。

自動確認には `--smoke` と単体テストを使う。端末経路はPTY上で文字入力、undo、
新規・既存ファイルの保存、dirtyな終了保護、raw mode / alternate screenの復元まで
確認する。
