# H.264 参数集依赖与激活语义分析：h264-reader 上下文模型评估

> 分析对象：本仓库 `h264-reader` 0.8.0（Rust，语法解析库，非解码器）
> 分析范围：SPS（NAL 7）、PPS（NAL 8）、subset SPS（NAL 15，MVC/SVC）、SPS 扩展（NAL 13）从读取、存储到被切片头、SEI、MP4 配置记录使用的完整链路，以及该链路与 ITU-T H.264 / ISO 14496-10 标准"参数集激活"语义之间的差距。
>
> **结论标记约定**
> - 【事实】：可由本仓库源码直接证实的行为，附 `文件:行号`。
> - 【推断】：基于源码行为与标准文本的合理推论，源码中没有直接的对应实现或测试。
> - 【缺口】：标准有规定但库未实现、或无法从源码确认覆盖的情形。

---

## 0. 结论速览

1. 【事实】库的 `Context` 只是三张以参数集 ID 为键的覆盖式 map（`src/lib.rs:17-21`），**没有"当前激活 SPS/PPS"的概念，没有版本/世代（generation）计数，没有失效或替换检测，也没有访问单元（AU）边界状态**。
2. 【事实】依赖解析全部采用"按 ID 现查"：PPS 解析时查 SPS（`pps.rs:340-342`）；切片头解析时先查 PPS、再经 PPS 内保存的 `seq_parameter_set_id` 查 SPS（`slice/mod.rs:509-516`）；`buffering_period` SEI 用载荷内自带的 sps_id 查 SPS（`buffering_period.rs:59-63`）；`pic_timing` SEI 则完全由调用方传入 SPS（`pic_timing.rs:245-249`）。
3. 【事实】同 ID 参数集重发/更新时，新值**静默覆盖**旧值（`lib.rs:85-90`），不校验新旧是否一致，也不标记任何引用旧值的 PPS/切片。
4. 【事实+推断】因此在真实码流的参数集变化情形下，错误表现为两类：
   - **可见报错**：引用的 ID 不存在（`UnknownSeqParamSetId` / `UndefinedPicParamSetId` / `UndefinedSeqParamSetId`），或结构型字段错位导致 RBSP 读取越界/对齐失败（`BitReaderError`）；
   - **静默错误值**：新参数集恰好保持位流对齐时，`frame_num` 宽度、POC 语义、图像尺寸、HRD 延迟字段、缩放矩阵等会按错误的参数集解释，库不报错。
5. 【事实】库自带的示例（`examples/dump.rs:113`）和 fuzz target（`fuzz/fuzz_targets/fuzz_target_1.rs:35-39`）对 `pic_timing` 都取 `ctx.sps().next()`（"第一个 SPS"），在多 SPS 码流下本身就是错的——这直接说明激活 SPS 的选择责任被推给了调用方，且库没有提供辅助手段。
6. 【事实】subset SPS 写入时会**同时克隆嵌入的基础 SPS 覆盖进普通 SPS map**（`lib.rs:67-71`），形成"后写者赢"的单一槽位，无法区分基础视图与非基础视图各自适用的参数集。
7. 【缺口】标准规定的同 ID 更新约束、图像内所有切片必须引用同一 PPS、AU 边界判定（7.4.1.2.4）、MVC VUI 操作点 HRD 选择、SVC/MVCD/深度参数集等情形均未建模或未实现。

---

## 1. 标准背景：参数集激活语义（基准，非源码内容）

本节陈述 H.264 标准的通用语义，作为后续差距分析的基准。

- 【事实（标准）】SPS ID 取值 0..31；PPS ID 取值 0..255。PPS 通过 `seq_parameter_set_id` 引用一个 SPS；切片头通过 `pic_parameter_set_id` 引用一个 PPS，从而间接绑定 SPS。
- 【事实（标准）】参数集是"带外/带内均可发送、可重发"的自包含语法结构。一个参数集在被 VCL NAL 引用时即"激活"：SPS 在引用它的 PPS 被切片使用时激活，PPS 在携带其 ID 的切片头处激活。
- 【事实（标准）】同一编码图像（access unit 的所有切片）必须使用同一个 `pic_parameter_set_id`；参数集可以在码流中更新，但标准对同 ID 更新施加约束（更新后的 SPS 不得改变仍被引用的若干字段，PPS 同理），合规码流不会在图像边界之外制造语义突变。
- 【事实（标准）】`buffering_period` SEI（D.2.1）载荷内带 `seq_parameter_set_id`，其 CPB 延迟字段的**个数与位宽**由该 SPS 的 VUI HRD 参数（`cpb_specs.len()`、`initial_cpb_removal_delay_length_minus1+1` 等）决定。
- 【事实（标准）】`pic_timing` SEI（D.2.2）的 `cpb_removal_delay`/`dpb_output_delay` 位宽、`pic_struct` 是否存在、clock timestamp 的 `time_offset_length`，均由**当前访问单元激活 SPS** 的 VUI/HRD 决定；`pic_timing` 载荷本身不含 sps_id，因此解析者必须先知道该 AU 的激活 SPS。
- 【事实（标准）】MVC（Annex G）/SVC（Annex F）中，subset SPS（NAL 15）与基础 SPS 共享 ID 空间但按视图/层适用；MVC VUI 扩展（G.14.1）按操作点（temporal_id + 目标视图集）携带独立的 timing/HRD 参数。
- 【事实（标准，ISO 14496-15）】MP4 的 AVCDecoderConfigurationRecord（avcC）在样本条目内携带初始 SPS/PPS 集合；样本条目变化意味着配置变化，消费方应以新配置重建解码上下文。

---

## 2. 库的上下文模型与完整依赖链路（源码事实）

### 2.1 存储结构：三张覆盖式 map

`Context`（`src/lib.rs:17-21`）：

```rust
pub struct Context {
    seq_param_sets: ParamSetMap<nal::sps::SeqParameterSet>,
    pic_param_sets: ParamSetMap<nal::pps::PicParameterSet>,
    subset_seq_param_sets: ParamSetMap<nal::subset_sps::SubsetSps>,
}
```

- 【事实】`ParamSetMap<T>` 是 `Vec<Option<T>>` 稀疏数组，`put` 按 index 直接覆盖（`src/lib.rs:85-90`），无返回值、无旧值比较、无世代记录。
- 【事实】ID 范围在类型层约束：`SeqParamSetId` 限 ≤31（`sps.rs:14-20`），`PicParamSetId` 限 ≤255（`pps.rs:303-309`）。
- 【事实】公开写接口：`put_seq_param_set`（`lib.rs:36-39`）、`put_pic_param_set`（`lib.rs:49-52`）、`put_subset_seq_param_set`（`lib.rs:67-71`）；公开读接口：`sps_by_id`/`pps_by_id`/`subset_sps_by_id` 及迭代器。**没有 remove/clear/replace 检测接口**（全仓 grep 无 `fn remove`/`fn clear`/`active` 状态）。
- 【事实】`put_subset_seq_param_set` 做双写：先把 `subset.sps.clone()` 写进普通 SPS map，再把完整 `SubsetSps` 写进 subset map（`lib.rs:67-71`），注释明确说是为了让 PPS/切片头经 `sps_by_id()` 找到它。

### 2.2 参数集的读取入口

- 【事实】Annex B 流由 `AnnexBReader` + `NalAccumulator` 做起始码切分（`annexb.rs`、`push/mod.rs`），切出的完整 NAL 交给用户闭包；**`Context` 本身不参与 NAL 排序，也看不到 NAL 单元类型的时间序列**——写入 `Context` 的动作发生在用户闭包里（如 `examples/dump.rs:54-71`）。
- 【事实】SPS 由 `SeqParameterSet::from_bits()` 无上下文独立解析（`sps.rs:1855-1859`）；PPS 由 `PicParameterSet::from_bits(&Context, ...)` 解析，签名即要求借用 Context。
- 【事实】avcC 由 `AvcDecoderConfigurationRecord::create_context()` 新建一个 `Context` 并依次灌入 SPS、PPS（`avcc.rs:184-201`）。

### 2.3 PPS 解析对 SPS 的依赖（解析期绑定）

【事实】`PicParameterSet::from_bits`（`pps.rs:335-385`）：

1. 读出 `pic_parameter_set_id` 与 `seq_parameter_set_id`；
2. 立即 `ctx.sps_by_id(seq_parameter_set_id)`，查不到则返回 `PpsError::UnknownSeqParamSetId`（`pps.rs:340-342`）——**这是显式可见错误**；
3. 借用该 SPS 完成一系列依赖解释：
   - slice group 矩形/分组的边界校验使用 `sps.pic_size_in_map_units()`、`sps.pic_width_in_mbs()`（`pps.rs:60-75, 121, 149, 183-189`）；
   - `PicScalingMatrix::read` 按 SPS 的 chroma_format 决定 8x8 列表形态（`pps.rs:239-261`）；
   - QP 范围校验使用 SPS 的 `bit_depth_luma_minus8`（`pps.rs:369-374`）。
4. 【事实】解析完成后，PPS 结构里**只保存 `seq_parameter_set_id` 这个整数**（`pps.rs:318`），不保存 SPS 的引用、指针或版本。此后 SPS map 中该 ID 的内容被替换，PPS 不会被重新校验。

### 2.4 切片头解析链路（使用期现查）

【事实】`SliceHeader::from_bits(ctx, r, header, header_extension)`（`slice/mod.rs:500-762`）的解析顺序：

1. 读 `pic_parameter_set_id` → `ctx.pps_by_id()`，缺失报 `UndefinedPicParamSetId`（`slice/mod.rs:509-513`）；
2. 经 `pps.seq_parameter_set_id` → `ctx.sps_by_id()`，缺失报 `UndefinedSeqParamSetId`（`slice/mod.rs:514-516`）；
3. 随后大量语法元素的**位宽、分支、取值**直接由查到的 SPS/PPS 决定，包括但不限于：
   - `colour_plane_id` 是否存在（SPS `separate_colour_plane_flag`，`slice/mod.rs:517-521`）；
   - `frame_num` 位宽 = SPS `log2_max_frame_num()`（`slice/mod.rs:522`）；
   - field_pic 分支（SPS `frame_mbs_flags`，`slice/mod.rs:523-535`）；
   - POC 语法形态完全由 SPS `pic_order_cnt` 驱动，delta 是否读第二个还取决于 PPS `bottom_field_pic_order_in_frame_present_flag`（`slice/mod.rs:552-593`）；
   - `redundant_pic_cnt`、`cabac_init_idc` 是否存在取决于 PPS 标志（`slice/mod.rs:594-598, 662-669`）；
   - 加权预测表 `PredWeightTable::read` 的循环次数取 PPS 默认参考帧数、chroma 分支取 SPS chroma 形态（`slice/mod.rs:289-377`）；
   - QP 校验用 SPS 位深（`slice/mod.rs:670-675`）；
   - 去块滤波字段是否存在取 PPS 标志（`slice/mod.rs:694-714`）；
   - `slice_group_change_cycle` 的位宽由 SPS 图像尺寸与 PPS change_rate 联合计算（`slice/mod.rs:715-728`）。
4. 【事实】函数返回 `(SliceHeader, &'a SeqParameterSet, &'a PicParameterSet)`（`slice/mod.rs:505, 761`），把本次实际使用的两个引用交还给调用方；借用期绑定 `&Context`，因此**单次调用内部** SPS/PPS 必然自洽，但跨 NAL 的一致性无任何保证。
5. 【事实】切片数据层 `SliceDataReader::new` 接收的是调用方传入的 `pic_size_in_mbs: u32` 等标量（`slice/data.rs:499-518`），不再回查 Context——即切片头解析时确定的参数被"快照"为普通数值继续使用。

### 2.5 SEI 链路：两种不同的 SPS 解析策略

**Buffering period（载荷自带 sps_id）**

- 【事实】`BufferingPeriod::read(ctx, msg)`（`buffering_period.rs:53-85`）先从载荷读 `seq_parameter_set_id` 并查 Context，缺失报 `BufferingPeriodError::UndefinedSeqParamSetId`（`buffering_period.rs:59-63`）；
- 【事实】随后用该 SPS 的 VUI HRD 参数决定读取列表：CPB 条目数 = `hrd.cpb_specs.len()`，每个延迟的位宽 = `initial_cpb_removal_delay_length_minus1 + 1`（`buffering_period.rs:65-79`）；
- 【事实】若 SPS 没有 VUI 或 HRD，则两个字段列表为 `None`，不读延迟，直接 `finish_sei_payload()`（`buffering_period.rs:72-80`）。
- 【推断】若 BP SEI 实际携带延迟载荷而查到的 SPS 无 HRD，`finish_sei_payload` 会因剩余字节触发 `BitReaderError::RemainingData`——表现为可见报错，但报错信息不会指出根因是"SPS 不匹配"。

**Pic timing（载荷无 sps_id，调用方全责）**

- 【事实】`PicTiming::read(sps, msg)` 直接要求调用方传入 SPS 引用（`pic_timing.rs:247-259`），文档注释明确写道："The caller is expected to have found the correct SPS by buffering the `SeiMessage` until after examining the following slice header."（`pic_timing.rs:245-246`）；
- 【事实】延迟位宽取 HRD 的 `cpb_removal_delay_length_minus1+1` / `dpb_output_delay_length_minus1+1`（`pic_timing.rs:261-287`）；`pic_struct` 仅在 VUI `pic_struct_present_flag` 为真时读取（`pic_timing.rs:289-308`）；
- 【事实】clock timestamp 的 `time_offset_length` 在 SPS 无 VUI/HRD 时**硬编码回退为 24**（`pic_timing.rs:203-213`）——这是库自造的默认值，标准中该长度只应来自 HRD；
- 【事实】库自身的两个调用示范都用 `ctx.sps().next()` 取"第一个 SPS"：`examples/dump.rs:113`、`fuzz/fuzz_targets/fuzz_target_1.rs:35-39`。多 SPS 码流下这等价于随机选择。

### 2.6 MP4 配置记录（avcC）链路

- 【事实】`create_context()` 把 avcC 内每个 SPS NAL 解析后 `put_seq_param_set`，再解析 PPS（此时 SPS 已在 Context 中）并 `put_pic_param_set`（`avcc.rs:184-201`）；
- 【事实】avcC 扩展区的 SPS extension NAL（NAL 13）仅作为字节迭代器暴露（`avcc.rs:159-169`），**`create_context` 不解析也不存储它们**；`Context` 也没有 SPS extension 的 map；
- 【事实】`ParamSetError::IncompatibleSps` 变体已定义（`avcc.rs:213`）但全仓**从未构造**——avcC 头部声明的 profile/level 与内嵌 SPS 之间不做任何一致性校验；
- 【事实】avcC 不处理 subset SPS（MVC 在 ISO 14496-15 中由独立的 mvcC/mvc1 配置承载，库亦未实现）。

### 2.7 subset SPS / 扩展 NAL 的位置

- 【事实】`SubsetSps::from_bits` 解析基础 SPS 数据 + SVC（83/86）/MVC（118/128/134）扩展，MVCD（135/138/139）扩展未解析（`subset_sps.rs:114-168`）；
- 【事实】MVC VUI 扩展（含每操作点 HRD）被完整解析进 `MvcVuiParametersExtension.ops`（`subset_sps.rs:391-438`），但全仓再无任何代码消费它（grep 仅命中定义文件）；
- 【事实】SVC 切片头在 `SliceHeader::from_bits` 中直接返回 `UnsupportedSyntax("SVC slice_header not supported")`（`slice/mod.rs:635-639`）；
- 【事实】SPS extension（NAL 13）可解析（`sps_extension.rs`）但无 Context 存储；DepthParameterSet（NAL 16）仅有枚举项，无实现。

---

## 3. 库上下文模型 vs 标准激活语义：差距清单

| # | 标准语义 | 库内现状 | 判定 |
|---|---|---|---|
| 1 | 参数集按 ID 引用、在 VCL 引用时激活 | 只有 ID→值的 map，无"激活"状态 | 【事实】缺口 |
| 2 | 同一图像所有切片引用同一 PPS | 逐 NAL 无状态解析，无图像级校验 | 【事实】缺口 |
| 3 | 同 ID 更新受标准约束，不得破坏在引参数 | 覆盖式 `put`，无新旧比较、无约束检查 | 【事实】 |
| 4 | 参数集持续有效直到被替换/会话结束 | 无失效/重置 API，仅靠 map 覆盖；无 EOS 处理 | 【事实】 |
| 5 | BP SEI 按载荷 sps_id 找 HRD | 已实现按 ID 自查（`buffering_period.rs:59`） | 【事实】符合 |
| 6 | pic_timing 按当前 AU 激活 SPS 解释 | 调用方传 SPS，库不跟踪 AU/激活集；示范用"第一个 SPS" | 【事实】不足 |
| 7 | AU 边界规则（7.4.1.2.4，首个 VCL/AUD） | Context 不消费 NAL 类型序列，无边界概念 | 【事实】缺口 |
| 8 | MVC/SVC subset SPS 按视图/层适用 | 双写进基础 SPS map，单槽位后写者赢 | 【事实】不足 |
| 9 | MVC 操作点 HRD（G.14.1） | 已解析但无选择/使用逻辑 | 【事实】缺口 |
| 10 | avcC 提供初始参数集，样本条目变更需重建上下文 | 仅一次性建 Context；变更检测与重建是调用方责任且无文档化辅助 | 【事实】 |
| 11 | PPS 语义依赖其引用 SPS，更新需保持一致 | PPS 只存 sps_id 整数；SPS 替换后不重新校验 PPS | 【事实】 |

---

## 4. 真实码流参数集变化场景：影响面与错误可见性

下列场景中，"可见报错"指返回 `Err`；"静默错误值"指解析成功但结果按错误参数集解释。

### 场景 A：SPS/PPS 尚未送达即被引用（乱序、截断、seek 后丢参数集）

- 【事实】PPS 解析引用未知 SPS → `PpsError::UnknownSeqParamSetId`（`pps.rs:340-342`）；
- 【事实】切片头引用未知 PPS → `UndefinedPicParamSetId`；PPS 存在但其 SPS 缺失 → `UndefinedSeqParamSetId`（`slice/mod.rs:509-516`）；
- 【事实】BP SEI 引用未知 SPS → `BufferingPeriodError::UndefinedSeqParamSetId`（`buffering_period.rs:61-63`）。
- **结论：可见报错，且定位准确。** 这是库覆盖得最好的一类。

### 场景 B：同 ID SPS 在 PPS 生成后被新内容覆盖（分辨率/格式变化但沿用 ID）

【事实】PPS 内只有 sps_id，切片头解析时拿到的是**新 SPS**。后果按变化字段分三类：

1. **改变切片头位流布局的字段**（`log2_max_frame_num_minus4`、`pic_order_cnt_type`、`frame_mbs_only_flag`、chroma/separate_colour_plane、各类 presence flag）：
   - 【推断】位宽/分支错位后，后续 Exp-Golomb 与定长读取大概率越界或对齐失败，经 `BitReaderError`（`UnexpectedEof`/`ExpGolombTooLarge`）或 `finish_rbsp` 的 `RemainingData` 表现为**可见报错**；
   - 【推断】但报错点远离根因——错误信息是某个字段读取失败，不会提示"SPS 已被替换"。调用方无法区分"码流损坏"与"参数集错配"。
2. **不改变布局、只改变语义的字段**（VUI/HRD 取值、cropping、timing、部分级别参数）：
   - 【事实+推断】解析全程成功，`pixel_dimensions()`、`fps()`、POC、`slice_group_change_cycle`（位宽公式含 SPS 图像尺寸，`slice/mod.rs:720-725`）等输出**静默错误值**，无任何告警。
3. **位深字段 `bit_depth_luma_minus8` 变化**：
   - 【事实】切片 QP 合法区间 `[-qp_bd_offset_y, 51]` 用新 SPS 位深计算（`slice/mod.rs:671-675`）；PPS 的 `pic_init_qp` 校验也用 SPS 位深（`pps.rs:369-374`）。
   - 【推断】可能把合法切片误判为 `InvalidSliceQpDelta`（假阳性报错），或放过按旧位深非法的值（假阴性静默接受）。
4. **缩放矩阵**：
   - 【事实】`PicScalingMatrix::scaling_lists_4x4/8x8(&ChromaInfo)` 是纯函数，SPS 回退规则完全取决于调用方传入哪个 SPS 的 `ChromaInfo`（`pps.rs:210-237`）。传入错配 SPS 时**静默返回错误的解析列表**，无错误路径。
5. **PPS 本身的合法性**：
   - 【事实】PPS 仅在 `from_bits` 时针对当时的 SPS 校验一次（slice group 边界等，`pps.rs:60-75, 121, 149`）；SPS 替换后无重新校验机制。【推断】可产生"PPS 对新 SPS 已非法但库仍正常返回切片头"的静默不一致。

### 场景 C：同 ID PPS 被覆盖

- 【事实】`put_pic_param_set` 直接覆盖（`lib.rs:49-52`），无世代信息；
- 【推断】若一个图像的多个切片跨越替换点（合规码流不应出现，但库不设防），不同切片会按不同 PPS 解释，库既不报错也无法检测；同 ID 新 PPS 若改变熵编码/权重/去块等 presence 标志，同样按场景 B 的机制产生可见错位或静默错误值。

### 场景 D：BP SEI 引用的 SPS 已被同 ID 更新

- 【事实】BP 按载荷内 sps_id 查到的是**当前 map 中的新 SPS**（`buffering_period.rs:59-63`），其 HRD 的 CPB 数量与字段位宽决定读取方式；
- 【推断】HRD 结构不同 → 位宽/个数错位 → 可见 `BitReaderError`；HRD 结构恰好相同仅数值不同 → 延迟值**静默错误**；新 SPS 无 HRD 而载荷含延迟 → `finish_sei_payload` 报错（见 2.5）。

### 场景 E：pic_timing SEI 与激活 SPS 错配

- 【事实】库不做 SPS 选择，`PicTiming::read` 拿到什么 SPS 就用什么（`pic_timing.rs:247`）；
- 【事实】官方示范与 fuzz 一律取第一个 SPS（`dump.rs:113`、`fuzz_target_1.rs:35`），多 SPS 码流下系统性错配；
- 【推断】后果：延迟位宽不符 → 可见 `BitReaderError`；`pic_struct_present_flag` 不符 → 多读/少读 4 bit，错位或 `RemainingData`；clock timestamp 在无 HRD 时走 24 bit 硬编码回退（`pic_timing.rs:209`）→ **静默错误时间戳**。

### 场景 F：subset SPS 与基础 SPS 同 ID（MVC/SVC）

- 【事实】`put_subset_seq_param_set` 把嵌入 SPS 克隆覆盖进普通 SPS map（`lib.rs:67-71`），此后基础视图切片经 PPS 查到的可能是 subset 嵌入的 SPS；两张 map 对同一 ID 的内容可能不同且无任何关联校验；
- 【推断】MVC 下标准要求同 ID 的基础/subset 内容保持一致，库的正确性**完全依赖码流合规**，自身无验证；SVC 下不同层可具有不同空间参数，单槽位模型无法表达层间差异，基础层切片可能拿到增强层 SPS，造成静默尺寸/格式错误；
- 【事实】切片头无法根据 NAL 头扩展中的 view_id/dependency_id 选择对应参数集——`SliceHeader::from_bits` 收到 `header_extension` 但仅用于 IDR 判定和 MVC 参考列表修改（`slice/mod.rs:536-546, 629-642`），不参与 SPS 选择；
- 【事实】MVC VUI 每操作点 HRD 解析后无人消费（`subset_sps.rs:391-438`），BP/pic_timing 也无法按 temporal_id/视图选 HRD——操作点级定时信息属于未覆盖缺口。

### 场景 G：MP4 样本条目变化 / 带内参数集更新

- 【事实】avcC 的 `create_context` 是一次性快照（`avcc.rs:184-201`）；若后续样本中带内出现同 ID 新 SPS/PPS 并写入同一 Context，行为等同场景 B/C；若调用方不写入带内更新，则 Context 永远停留在 avcC 快照（静默使用过期参数，库同样无告警）；
- 【事实】avcC 头与 SPS 的 profile/level 一致性检查缺失（`IncompatibleSps` 定义未用，`avcc.rs:213`）；
- 【推断】样本条目切换时调用方若复用旧 Context，所有在引切片都可能按旧配置解释；库没有提供"配置代际"检测来帮助调用方发现这种切换。

### 场景 H：错误可见性汇总表

| 受影响解析结果 | 错配触发方式 | 表现 | 源码依据 |
|---|---|---|---|
| PPS 解析 | 引用 SPS 不存在 | 可见 `UnknownSeqParamSetId` | `pps.rs:340-342` |
| 切片头 | PPS/SPS 不存在 | 可见 `Undefined*Id` | `slice/mod.rs:509-516` |
| 切片头位流布局 | 结构型 SPS/PPS 字段变化 | 多为可见 `BitReaderError`，但根因不可定位 | 【推断】 |
| `frame_num`/POC/字段 presence | 同 ID 覆盖且布局未错位 | 静默错误值 | `slice/mod.rs:522, 552-598` |
| QP 合法性 | 位深变化 | 假阳性报错或假阴性放行 | `slice/mod.rs:670-675`、`pps.rs:369-374` |
| 图像尺寸/裁剪/fps/VUI | SPS 覆盖 | 静默错误值 | `sps.rs:1888-1977` |
| 缩放矩阵回退 | 传入错配 SPS ChromaInfo | 静默错误列表 | `pps.rs:210-237` |
| BP SEI 延迟 | sps_id 指向被更新 SPS | 可见错位或静默错值 | `buffering_period.rs:65-79` |
| pic_timing | 调用方传错 SPS | 可见错位或静默错值；无 HRD 时 24bit 回退 | `pic_timing.rs:203-213, 261-287` |
| MVC/SVC 视图参数选择 | subset 双写覆盖 | 静默，且无视图感知 | `lib.rs:67-71` |
| avcC 一致性 | 配置与 SPS 不符 | 无检查（错误变体未使用） | `avcc.rs:213` |

---

## 5. 兼顾现有 API 兼容的改进方案

### 5.1 设计原则

- 【事实】`ParamSetMap` 是库私有类型（`lib.rs:75`，无 `pub`），可自由改内部结构而不破坏 API；
- 所有现有公开方法签名保持不变，新能力以**新增方法/新增类型**方式提供（additive change），不修改既有 `Result` 类型；
- 库定位是语法 reader 而非解码器，不强制调用方接受新状态机，激活语义做成"可选层"。

### 5.2 状态应保存在哪里

1. **世代（revision）信息存入 map 条目**（私有改动）：

   ```rust
   struct ParamSetEntry<T> { value: T, revision: u64, replaced_count: u64 }
   ```

   `Context` 内维护单调递增的全局 `revision: u64`，每次 `put_*` 自增。`SeqParameterSet` 已派生 `PartialEq`（`sps.rs:1784`），可在覆盖前比较：内容相同（纯重发）→ 不晋升世代；内容不同（真更新）→ 晋升并记录旧世代。
2. **激活快照放在新增的可选 AU 跟踪器中**，而不是塞进现有 map：新增 `AccessUnitTracker`（或 `ActivationState`），由调用方在每个 NAL 到达时喂入 `UnitType`/NAL 头，内部按 7.4.1.2.4 判定 AU 边界，在首个 VCL 切片解析后记录 `(pps_id, sps_id, pps_revision, sps_revision, view_id, temporal_id)` 快照。
3. **PPS 反向依赖标记留在 Context**：SPS 条目上维护"引用我的 PPS id 集合"（PPS `put` 时登记），SPS 真更新时把这些 PPS 标记为 `stale`，不删除（标准允许新旧共存，旧图像已用旧集合解析完毕）。

### 5.3 具体新增 API（建议）

- `PutOutcome { Inserted, Unchanged, Replaced { previous_revision } }`：`put_*` 保留原签名，新增 `put_*_tracked()` 返回 outcome；或新增 `Context::sps_revision(id)/pps_revision(id)` 查询。
- `Context::reset()`：显式清空全部参数集（seek、切换会话、avcC 样本条目变化时调用）。
- `Context::is_pps_current(pps_id) -> bool` / `pps_staleness(pps_id) -> Option<Staleness>`：暴露 5.2 的 stale 标记。
- 切片解析保留 `SliceHeader::from_bits`；新增 `from_bits_tracked()`，返回值附带 `ParseWarnings { stale_pps, sps_replaced_after_pps, revision_mismatch }`，把今天的静默错配升级为**结构化告警**（仍不阻断解析）。
- SEI：
  - `PicTiming::read(sps, msg)` 签名不动；新增 `Context::resolve_pic_timing_sps(au_snapshot)` 之类辅助，或在 `AccessUnitTracker` 上缓存 AU 内 SEI、待切片头揭示激活 SPS 后再解析（落实 `pic_timing.rs:245-246` 注释目前只靠口头约定的流程）；
  - `BufferingPeriod::read` 增加 HRD 存在性与字段消费一致性检查，把 2.5 的隐式 `RemainingData` 变为带上下文的错误变体。
- subset SPS：保留 `put_subset_seq_param_set` 现有双写行为（兼容），新增 `put_subset_seq_param_set_scoped()` 只写 subset map，并新增视图感知查询 `sps_for_view(id, ViewSelector { base, view_id, temporal_id })`；切片 `from_bits_tracked` 结合 `NalHeaderMvcExtension.view_id()` 自动选择。
- avcC：新增 `create_context_checked() -> (Context, ConfigReport)`，报告 profile/level 一致性、PPS→SPS 链接完整性、扩展 NAL 情况；顺手让 `IncompatibleSps`（`avcc.rs:213`）真正被产出；SPS extension 增加 Context 存储 map 与 `put_sps_extension()`。

### 5.4 何时失效

| 事件 | 动作 |
|---|---|
| 同 ID 同内容重发 | 不晋升 revision（幂等），不标记 stale |
| 同 ID 异内容 SPS | 晋升 SPS revision；标记引用它的 PPS 为 stale；不删旧值 |
| 同 ID 异内容 PPS | 晋升 PPS revision；下一切片解析时可经 warning 发现跨图像替换 |
| `Context::reset()`（新方法） | 清空三张 map 与世代计数（seek/会话切换/样本条目变更） |
| AU 边界（跟踪器判定） | 固化上一 AU 激活快照，开始新快照；校验同图像 PPS 一致性 |
| 新 avcC / 新 sample entry | 调用方 reset 后重建，或由新配置生成新 Context |

---

## 6. 必须留给调用方的责任

以下职责无法、也不应收进库内，改进方案只提供辅助、不替代决策：

1. **Context 生命周期管理**：一个流会话一个 Context；seek、切节目、MP4 样本条目变化时由调用方 reset/重建——库无法知道分片传输的语义边界。
2. **NAL 投喂顺序与 AU 边界**：push 架构下 NAL 顺序由调用方保证；是否启用 `AccessUnitTracker`、如何处理 AUD 缺失的码流，由调用方选择。
3. **pic_timing 的 SEI 缓存**：SEI 在切片头之前到达且载荷无 sps_id，"缓存 SEI 直到切片头揭示激活 SPS"的缓冲策略天然属于调用层（库只提供 SPS 解析辅助）。
4. **告警策略**：收到 `StalePps`/revision 不匹配后是丢帧、等 IDR、仅记日志还是继续解析，属于应用策略。
5. **MVC/SVC 视图与操作点选择**：播放哪个视图、哪条 temporal 子链是应用/容器层决策；库据此提供参数集选择原语，但不内置播放策略；MVC VUI 操作点匹配规则的调用时机也由调用方驱动。
6. **MP4 封装侧职责**：哪些参数集进 avcC、哪些带内发送、样本条目何时切换，是 muxer/demuxer 的责任；库只保证配置记录可被完整解析并报告一致性。
7. **标准合规性裁决**：超出语法取值范围的"shall"约束（如同 ID 更新允许改哪些字段、参考图像管理后果）是解码器/合规校验器的职责；本库作为语法 reader 只负责把错配从静默变为可观测。

---

## 7. 尚未覆盖的标准情形（缺口清单）

1. 【缺口】激活参数集语义整体缺失：无 active SPS/PPS、无图像内 PPS 一致性检查、无 AU 边界状态机（标准 7.4.1.2.4、7.4.2）。
2. 【缺口】同 ID 参数集更新的标准约束未建模：库无法区分"合规重发/更新"与"破坏性替换"，也无 PPS 对新 SPS 的重新校验。
3. 【缺口】`pic_timing` 激活 SPS 解析无库内支持；官方示范的"第一个 SPS"策略在多 SPS 码流下错误（`dump.rs:113`、`fuzz_target_1.rs:35`）。
4. 【缺口】MVC/SVC 视图作用域参数集选择；MVC VUI 操作点 HRD（G.14.1）解析后未被任何代码使用；SVC 切片头直接拒绝（`slice/mod.rs:636-638`）。
5. 【缺口】MVCD subset 扩展跳过未解析（`subset_sps.rs:145-150`）；DepthParameterSet（NAL 16）与 3D-AVC 未实现。
6. 【缺口】SPS extension（NAL 13）可解析但 Context 无存储；avcC 扩展区的该类 NAL 不进 `create_context`。
7. 【缺口】数据分区切片（NAL 2/3/4）未实现，其 PPS 激活链路同样未覆盖。
8. 【缺口】`end_of_seq`（NAL 10）/`end_of_stream`（NAL 11）对参数集生命周期的影响未建模（仅枚举了 NAL 类型）。
9. 【缺口】参数集留存策略：无容量上限/淘汰（PPS ID 可达 255，map 随 ID 增长），标准外的长期运行会话内存增长无防护。
10. 【缺口】HRD 选择语义：NAL HRD 与 VCL HRD 的适用场景区分（BP SEI 读哪一套）依赖比特流合规点，库目前两套都读、不做场景区分；`time_offset_length` 的 24 bit 回退（`pic_timing.rs:209`）是库造默认值而非标准规则。
11. 【缺口】测试覆盖：全仓测试没有任何使用非零 SPS/PPS ID 的用例（grep 无 `from_u32(1/2)` 构造），多参数集、参数集替换、跨 AU 错配场景零覆盖。

---

## 8. 附：关键源码位置索引

| 主题 | 位置 |
|---|---|
| Context 三张 map 与覆盖式写入 | `src/lib.rs:17-21, 36-52, 85-90` |
| subset SPS 双写 | `src/lib.rs:64-71` |
| PPS 解析期 SPS 绑定与校验 | `src/nal/pps.rs:335-385` |
| PPS 仅存 sps_id 整数 | `src/nal/pps.rs:318` |
| 切片头 PPS→SPS 现查与全部依赖点 | `src/nal/slice/mod.rs:500-762` |
| 切片头返回所用 SPS/PPS 引用 | `src/nal/slice/mod.rs:505, 761` |
| 切片数据层只收标量快照 | `src/nal/slice/data.rs:499-518` |
| BP SEI 按载荷 sps_id 自查 | `src/nal/sei/buffering_period.rs:53-85` |
| pic_timing 调用方传 SPS + 24bit 回退 | `src/nal/sei/pic_timing.rs:203-213, 245-259` |
| avcC 建 Context / 未用的一致性错误 | `src/avcc.rs:184-201, 213` |
| MVC VUI HRD 解析后无消费 | `src/nal/subset_sps.rs:391-438` |
| SVC 切片头拒绝 | `src/nal/slice/mod.rs:635-639` |
| 示范/fuzz 的"第一个 SPS"策略 | `examples/dump.rs:113`、`fuzz/fuzz_targets/fuzz_target_1.rs:35-39` |
