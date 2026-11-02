# 写一个 skill

skill 是 agent **可以选择遵循**的 Markdown。它是指令，不是程序，也不是权限：它说的话与 agent
的其他动作受同一道沙箱约束，也不能把沙箱拓宽。这也是它不签名、不版本化、不被信任的原因 ——
一个 skill 写起来便宜，正因为它并不比 agent 自己的判断更有分量。

deepnest 读的是 Anthropic Agent Skills 格式的那两种形态。本页写的是 `crates/skills` 在那些文件
上究竟做了什么，顺序按「要写一个 skill 的人需要它的先后」排。

## skill 放在哪

有两个根，每一轮都读：

| 根 | 属于谁 | 作用范围 |
|---|---|---|
| `~/.deepnest/skills/` | 你 | 所有 app 共享 |
| `<app>/.agent/skills/` | 某一个 app | 只对它自己 |

**app 的 skill 会遮蔽同名的用户 skill。** 方向是刻意选的：一个 app 需要不一样的 `notes` skill
时，不该逼你给自己的那个改名。根不存在不算错误 —— 首次运行还没建过它，为此拒绝启动会让新用户
看到的第一件事就是失败。

## 两种形态，以及谁赢

两者是同一件事的两种写法：

```
~/.deepnest/skills/notes.md            # 单文件
~/.deepnest/skills/notes/SKILL.md      # 目录
```

目录可以带上 skill 引用的脚本与资产，所以当单个文件装不下时，该搬到的就是目录形态。**同一个根里
两种写法声明了同一个名字时，目录赢** —— 它能装的东西更多，正在迁移过去的 skill 因此中途仍然可用。
被它比下去的那个文件是**被报告，而不是被丢弃**：改了 `notes.md` 却毫无变化，否则你手上就没有任何
可以看的东西。

## 头部

skill 是「几行头部 + 指令」的 Markdown：

```markdown
---
name: notes
description: Use when the user asks about their notes, tags or notebooks.
---

# Instructions

…要做什么，用 agent 能执行的话写…
```

`description` **必填，且不能为空**。它是 agent 挑不挑这个 skill 的唯一依据，所以没有它的 skill
无法被选中 —— 它不会被加载，面板会说明原因。`name` 可以不写：它回落到文件名（`notes.md` →
`notes`）或目录名（`notes/SKILL.md` → `notes`）。

关于头部有四条规则，每一条都是因为另一种做法的代价已经付过了：

- **缩进的行不是字段。** 一段嵌套内容 —— 比如 `metadata:` 列表 —— 不能覆盖顶层的
  `description`。把所有带冒号的行都当字段读，正是一个 skill 最后用自己文件更靠下的某句话来描述
  自己的原因。
- **结尾的 `---` 是一行，不是一次出现。** description 里含 `---` 没关系；头部在文件说它结束的
  地方结束。
- **带引号的值会脱引号，其余一概不解释。** `name: "notes"` 就是 `notes`。没有转义二次处理，
  因为名字和描述是给 agent 读的文本，不是任何东西会重新解析的格式。
- **同名字段以第一次出现的为准。** 更靠下的第二个 `description:` 不会替换第一个。

## agent 看得到什么

prompt 里拿到的是每个 skill 的**名字、描述和路径**，**永远不是正文**。agent 自己在那条路径上读
文件，在它判断这个 skill 相关时。这是渐进披露，也是为什么十几个 skill 还能被列出来的全部理由：
十几份指令会把对话挤掉，而十几个名字加描述只花一段话。

由此可知，**描述比名字更决定成败**。把它写成「什么时候该用这个 skill」的条件。

## 被报告而不是被加载的那些

加载器不会大声失败，也不会悄悄失败。它拒绝的每一个条目都带着路径和原因回来，面板把它们列在加载
成功的那些旁边：

| 你写的东西 | 回来的结果 |
|---|---|
| 目录里没有 `SKILL.md` | 报告：里面没有 `SKILL.md`，所以它不是 skill |
| 不是 `.md` 的文件 | 报告：不是 Markdown，所以不可能是 skill |
| 头部解析不了 | 报告，并给出解析器的原因 |
| 没有 `description`，或它是空的 | 报告：agent 没有可用来挑它的依据 |
| 软链接 | 报告，**不跟随** ——「这东西哪来的」是一个 skill 列表必须答得出的问题 |
| 读不了的条目 | 报告，并给出操作系统的错误 |

被跳过的 skill 不等于坏掉的 skill：其余的照常加载。一张空列表旁边是一串拒绝原因，意思是「你的
skill 有笔误」；这与「还没有任何 skill」是两句话。

## 怎么检验

打开 `http://127.0.0.1:4270/`，看 Skills 那一节。它每次请求都重新渲染而不是缓存，所以改完文件
刷新页面就能看到变化 —— 这就是「改完 skill 之后要看的那一页」。

那一页不属于任何 app，所以只列**用户的** skill。app 自己在 `<app>/.agent/skills/` 下的 skill
不在其中：把所有 app 的 skill 合成一张表，等于宣称每一个都处处适用。

## 一个完整的例子

一个带上脚本、并希望脚本被执行的 skill：

```
~/.deepnest/skills/release-notes/
├── SKILL.md
└── notes.sh
```

```markdown
---
name: release-notes
description: Use when the user asks for release notes or a changelog entry.
---

# Release notes

运行与本文件同目录的脚本：

    bash ~/.deepnest/skills/release-notes/notes.sh

然后把它打印的内容整理成摘要，按每条提交带来的用户可见变化分组。
```

正文里的路径刻意写成绝对路径：agent 被告知的是文件在哪，而不是它相对于什么东西在哪。
