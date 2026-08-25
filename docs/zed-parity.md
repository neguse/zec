# Zed experience parity contract

Status: Accepted (2026-08-26)

## Goal

zecの長期目標は、Zedが持つ編集、language intelligence、workspace、Git、実行・debug、
extension、remote、AI、collaborationのworkflowをconsoleから完結できるようにすることである。
pixel単位でGPUIを再現することは目標にせず、同じZed modelへ同じ操作を適用し、同じ永続状態と
外部作用を得られることをparityとする。

対象機能と到達状況のnormativeな台帳は
[`zed-parity-v1.json`](zed-parity-v1.json)で管理する。文書中の完了表現、issue、手動dogfoodだけで
台帳を`verified`へ変更してはならない。

## Delivery modes

各capabilityは次のいずれかで提供する。consoleで表現しにくいことを理由にcapability自体を
削除する`excluded` modeは設けない。

- `faithful`: Zedのdomain modelとactionをauthorityとして使い、terminalは入出力だけを変換する。
- `terminal-adapted`: 同じstate/actionを、cell、text、terminal image protocolなどへ投影する。
- `external-bridge`: audio、screen share、browserなどterminalが所有できない媒体を、明示的な
  processまたはOS serviceへ接続し、zec内では状態、権限、開始・停止、errorを操作できるようにする。

端末能力が足りない場合は、利用可能なmodeへfallbackするか、必要な能力と代替操作を表示する。
入力を無視したり、成功していない操作を成功表示したりしてはならない。

## Architectural invariants

1. text、cursor、selection、transaction、undo、display mapはZed Editor/Bufferをauthorityにする。
2. worktree、LSP、Git、tasks、DAP、settings、toolchain、remote projectはZed Projectのstoreを
   authorityにする。zec独自の並行modelへ同期しない。
3. pane、item、focus、modal、navigation historyは1つのTerminal Workspace modelから操作する。
   featureごとに独立したevent loopやfocus flagを増やさない。
4. rendererはimmutable snapshotだけを読み、frame描画中にdomain stateを変更しない。
5. Zedの公開APIが不足する場合は、logicをzecへcopyする前に小さなpresentation-neutral hookを
   upstreamへ追加できる形にする。patchは固定Zed revisionごとにtestする。
6. network、process execution、extension、agent、collaboration、remote接続は権限境界を持ち、
   silent elevationを行わない。
7. 各milestoneはそれ以前のcanonical gateを完全に再実行する。後段機能による回帰を既知制約として
   bypassしない。

## Terminal Workspace primitives

後続featureが共有するpresentation型を次に固定する。

- `Item`: Editor、MultiBuffer、Terminal、Diff、Markdown、Image、Notebook、Settings
- `Layout`: tab strip、split tree、dock、panel、overlay stack
- `Overlay`: prompt、completion、hover、menu、picker、confirmation、notification
- `Collection`: list、tree、table、virtualized result set
- `Action`: Zed action ID、key context、availability、dispatch result
- `Capability`: keyboard protocol、color、mouse、clipboard、image、hyperlink、focus event

Quick Open、検索、Save Asなど既存のsingle-purpose UIは、対応する共通primitiveが導入された
milestoneで同じreducerへ移す。移行中も本文やZed stateをprimitive側へ複製しない。

## Milestones

### Alpha 1: repository editing loop

file discovery、basic project search、複数file編集、保存、terminal lifecycleを保証する。
既存の[`alpha-1.md`](alpha-1.md)をnormative contractとする。

### Alpha 2: Project-backed language editing loop

Zed Project、settings、command palette、LSP、completion、diagnostics、navigation、code action、rename、
format、language系MultiBufferをconsoleから完結させる。normative contractは
[`alpha-2.md`](alpha-2.md)とする。

### Alpha 3: terminal workspace

pane/dock、project panel、outline、breadcrumbs、完全なproject search/replace、navigation history、
advanced editingとsession restoreを共通Terminal Workspace上へ統合する。

### Beta 1: local development loop

Git、integrated terminal、tasks、test、DAP debugger、REPL/notebookとcrash recoveryを保証する。

### Beta 2: ecosystem and remote

extension、theme、全keymap、package/update、large-file path、rich content、SSH/WSL/dev-container、
Linux/Windows/macOSの配布と互換性を保証する。

### Parity 1: AI and collaboration

Zed Agent、external ACP agent、MCP、skills/instructions、edit prediction、inline assistant、parallel agent、
real-time共同編集、channels、following、notesを統合する。voiceとscreen shareは
`external-bridge`として権限とlifecycleをzecから操作する。

## Completion rule

長期目標の完了は次をすべて満たした時だけ宣言する。

1. `zed-parity-v1.json`の全capabilityが`verified`である。
2. 各capabilityのevidenceがdefault branch上のretryなしのcanonical CI runを指す。
3. pinned Zed revisionとの差分監査で新しいuser-facing domainが未登録ではない。
4. fresh installから各platformの総合actual-binary scenarioが成功する。
5. terminal capability不足時のfallbackとerrorがPTY/ConPTY試験で検証される。
6. source of truth、process cleanup、data safety、権限境界の全invariantが回帰gateを通る。
