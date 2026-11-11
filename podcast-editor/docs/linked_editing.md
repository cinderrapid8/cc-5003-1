# 多轨联动剪辑 · 使用与运行说明

本文说明播客剪辑台新增的「主持人 / 嘉宾 / 现场气氛」三轨联动剪辑能力，
覆盖核心概念、典型用例、HTTP 接口与本地运行/验证方式。
所有时间位置一律以**工程采样帧整数**表达，系统中不存在毫秒字段。

## 1. 核心概念

| 概念 | 说明 |
| --- | --- |
| 轨道 Track | 新建工程自动创建三条角色轨 `track_host` / `track_guest` / `track_ambience`。每轨有独立录音起点偏移 `start_offset`（可为负）、`hidden`（隐藏）、`locked`（锁定）。 |
| 片段 Item | 对原始录音的只读引用（`source_start`/`source_end` 源帧 + `timeline_start` 工程帧）。编辑只改引用，永不改写源文件。`timeline_start` 允许为负。 |
| 剪辑组 EditGroup | 把多轨上时间对齐的片段绑定为一个联动单元，支持嵌套（`parent_id`），嵌套关系禁止成环。成员经子组递归展开。 |
| 锁定 locked | 硬约束。任何会改动该轨片段位置/范围的操作（含联动波纹、整组移动、分组淡化）一律拒绝，并返回精确到轨/组的冲突原因；隐藏的锁定轨同样一票否决。 |
| 隐藏 hidden | 仅界面状态。隐藏轨的片段仍是组成员，参与全部联动规则，不会因为未显示而失去同步。 |
| 零点策略 ZeroPolicy | 当操作会产生/加深负起点时必须显式选择：`reject` 整单拒绝且零变更；`clip` 把越界部分从引用范围裁掉、起点钳到 0。 |
| 版本快照 | 每次联动编辑只写一个新版本快照。一次撤销即恢复该快照下全部受影响轨道。 |

### 波纹删除（ripple delete）语义

- `ripple=false`：仅删除目标片段，其他片段的工程位置一帧不动。
- `ripple=true` 且片段属于某组：整组（含子组）联动。
  - 完全落在删除区间内的成员被删除；
  - 与区间部分相交的成员按真实源帧裁切；**横跨整个区间的长片段被裁切后，
    右侧保留在原边界帧**（它引用的素材本就不在被删空隙内）；
  - 位于区间之后的独立成员整体前移 `delta`；
  - 裁切/拆分派生的新片段**继承原片段的组成员身份**，不会脱离联动。
- `ripple=true` 且片段不属于任何组：仅在本轨波纹，组外片段一律不动。

### 交叉淡化

- 淡化只在**同一轨道相邻片段**之间成立，按每条轨道各自的真实素材余量计算。
- 分组交叉淡化会统一评估组内每轨相邻对：任一轨余量不足即整单拒绝，
  绝不通过缩短淡化去偷偷改变整组时长。
- 混音保留真实线性叠加（三轨同帧脉冲相加为 3.0）；落盘 16-bit PCM 时
  由 WAV 写出层统一限幅，分轨各自单轨天然落在 [-1,1]。

## 2. 浏览器界面用例（static/index.html）

启动后打开 `http://127.0.0.1:8080/`。

1. **新建工程**：自动带主持人/嘉宾/气氛三轨。
2. **轨道面板（左上）**：每条轨道可一键 锁定/隐藏、设置录音起点偏移（可负）。
3. **加入片段**：选择素材、目标轨道、源帧范围与时间线起点（可负，留空则接该轨末尾）。
4. **多轨泳道**：按轨分色显示片段，红色虚线为工程零点，负起点片段用虚线框标识；点击片段选中。
5. **单轨截取 / 切分**：只改该片段引用，其他轨道工程位置原样保留；切分右片段继承同轨与组身份。
6. **联动删除**：选中片段后选择零点策略（reject/clip），执行「非波纹删除」或「波纹删除」。
7. **剪辑组卡片**：展示联动范围、成员轨、锁定轨、隐藏仍联动标记与全部冲突原因；
   支持整组移动（带零点策略）、分组交叉淡化、设置嵌套父组（成环会被拒绝）、加入选中片段。
8. **导出**：「导出混音 WAV」或「导出分轨」（混音 + 每轨 WAV，全部引用同一工程修订版本）。
9. **撤销/重做**：在快照间切换，一次撤销恢复所有受影响轨道。

## 3. HTTP 接口速览

所有写操作都是「纯函数算出新工程 → 成功后一次性写一个版本快照」，
中途任何冲突都在写入前返回，库里的当前修订不受影响。

| 方法 & 路径 | 作用 |
| --- | --- |
| `POST /api/projects` | 新建三轨工程 |
| `GET/POST /api/projects/{pid}/tracks`、`POST /api/projects/{pid}/tracks/{tid}` | 列出轨道 / 更新 locked、hidden、start_offset |
| `POST /api/projects/{pid}/items` | 加片段（带 `track_id`、可选负 `timeline_start`） |
| `POST /api/projects/{pid}/items/delete` | 联动删除 `{item_id, ripple, zero_policy}` |
| `POST /api/projects/{pid}/trim` / `split` | 单轨截取 / 切分（不动他轨） |
| `GET/POST /api/projects/{pid}/groups` | 列组 / 建组 |
| `POST /api/projects/{pid}/groups/{gid}/items` | 片段入组 |
| `POST /api/projects/{pid}/groups/{gid}/parent` | 设置嵌套父组（成环拒绝） |
| `POST /api/projects/{pid}/groups/{gid}/move` | 整组移动 `{delta, zero_policy}` |
| `POST /api/projects/{pid}/groups/{gid}/crossfade` | 分组交叉淡化（按各轨真实余量） |
| `GET  /api/projects/{pid}/groups/status` | 联动范围/锁定轨/隐藏轨/冲突原因 |
| `POST /api/projects/{pid}/export/stems` | 混音 + 每轨 WAV，统一版本 |
| `POST /api/projects/{pid}/undo` / `redo` | 快照级撤销/重做 |

## 4. 本地运行

```bash
# 工具链：Rust（edition 2024）。如 cc 不可用，确保 gcc 在 PATH 中。
cargo build
# 数据目录与监听地址可用环境变量覆盖
PE_DATA_DIR=./run_data PE_BIND=127.0.0.1:8080 \
  ./target/debug/podcast_editor
# 浏览器打开 http://127.0.0.1:8080/
```

## 5. 自动化验证

### Rust 单元/集成测试（68 个）

```bash
cargo test
```

- `tests/linked_editing.rs`（29 个）覆盖：三轨同脉冲多次波纹后相对位置不变、
  锁定轨零变更否决、隐藏轨仍参与联动、负起点 reject/clip、嵌套成环、
  分组交叉淡化逐轨余量、批量移动单快照可整单撤销、旧单轨工程免迁移打开、
  缺失源文件渲染前报错、混音与分轨同修订逐样本一致、空组回收等。
- `tests/e2e.rs`（23 个）与 `src` 内单元测试（16 个）保持原有能力不回归。

### HTTP 端到端工作流（78 项检查）

```bash
# 终端 1：起服务
PE_DATA_DIR=/tmp/pe_run PE_BIND=127.0.0.1:8080 \
  ./target/debug/podcast_editor
# 终端 2：跑脚本（缺失源文件用例会自行在 8091 端口拉起/冷重启一个实例）
python3 -I scripts/e2e_http.py
```

脚本第 11 节专门验证多轨联动：三轨脉冲对齐、锁轨否决、隐藏轨随动、
负起点策略、分组淡化、嵌套成环、分轨同修订导出、一次撤销恢复、
以及冷重启后缺失源文件在渲染前被明确拦截。

## 6. 旧单轨工程兼容性

旧快照没有 `tracks`/`groups`、片段也没有 `track_id`。载入时执行 `normalize_legacy`：
补一条 `track_legacy` 轨道并把无轨片段挂上去，**素材与源文件不做任何转换或拷贝**，
随后即可在同一界面中查看与编辑。相关用例见
`tests/linked_editing.rs::legacy_single_track_project_opens_without_migration`。
