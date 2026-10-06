---
name: deep-research
description: "Structured multi-agent investigation: confirms what the user actually needs, decomposes it into subjects × angles, researches each cell in a parallel sub-agent with its own search brief, then merges and reports. Use when the user asks to research / 调研 / investigate / compare products, papers, libraries or vendors — anything needing breadth across many sources, not a single lookup."
descriptions:
  en-US: "Structured multi-agent investigation: confirms what the user actually needs, decomposes it into subjects × angles, researches each cell in a parallel sub-agent with its own search brief, then merges and reports. Use when the user asks to research / investigate / compare products, papers, libraries or vendors — anything needing breadth across many sources, not a single lookup."
  zh-CN: "结构化多 agent 调研：先确认用户真正要解决的问题，再拆成「主体 × 方向」的二维矩阵，每个格子派一个并行子 agent 带着各自的检索策略去查，最后由 main 合并成报告。当用户说调研、深入研究、对比一批产品/论文/库/厂商，或任何需要跨多来源铺开而不是单次查询的任务时，用它。"
---

# Deep research

Structured investigation as a fan-out / merge: decompose, research every cell in parallel, then
reassemble.

## The two ideas this is built on

**Confirm before decomposing.** The most common way a deep investigation goes wrong is not a bad
search — it is researching the wrong question thoroughly. A user saying "调研一下 RAG 方案" may
want a comparison to pick one, a survey to write a paper section, or a sanity check on a decision
they already made. These three want different subjects and different fields. Ask which, before
building anything.

**The unit of parallelism is a cell, not a subject.** Decomposing "compare three AI coding agents"
into three subjects gives each subject exactly one agent, and one agent writing a pricing section, a
capability section and a reception section from a single reading of the web produces one
perspective, repeated three times. Decomposing it into **subjects × angles** fixes this: pricing,
capabilities and reception each get their own agent with their own search brief, so a claim about
price is checked on the vendor's own pricing page while a claim about capability is checked in the
docs and a claim about reception is checked where people complain. Then main merges each subject's
angles back into one view and can see where they disagree.

---

## What lives on disk

```
{workspace}/{topic-slug}/
  outline.yaml              # subjects × angles + the execution plan
  fields.yaml               # the schema each cell fills
  results/
    {subject-slug}/
      {angle-slug}.json     # one file per cell — the unit of parallel work
      merged.md             # main's merge of that subject's angles
  report.md                 # the assembled report
```

The `results/{subject}/` grouping is what makes the merge possible: one subject's angles are all in
one directory.

---

## Phase 1 — confirm, then decompose

Nothing is written until the user has confirmed the decomposition.

### Step 1: find out what the answer is for

Ask, with `ask_user`, before drafting anything:

- **What will you do with this?** Pick one, because each implies a different investigation:
  - *Choose one* → comparative fields, decision criteria, a recommendation at the end
  - *Write something* → breadth and citation coverage matter more than depth
  - *Understand a field* → concepts, mechanisms, how the pieces relate
  - *Check a decision already leaning* → disconfirming evidence matters most; actively look for
    what argues the other way
- **Time range** — "last 12 months", "since 2024", "no limit". Without this, a sub-agent cannot
  tell a launch announcement from last year from a product that has since been rewritten.
- **Anything explicitly out of scope** — a competitor they do not want, a language, a price band.

A one-line restatement of what you understood, plus these questions, is enough. Do not draft a
20-subject outline before you know the answer.

### Step 2: draft subjects and angles

**Subjects** are the things being investigated — named concretely. "Major AI coding agents" is not
a subject; Claude Code, Cursor and GitHub Copilot are.

**Angles** are the directions. Pick per subject, not globally — a simple subject may need one
angle, a contested one needs three:

| Angle | What it establishes | Where to look |
|---|---|---|
| `basics` | Who made it, when, what it is | Official site, company page, release notes |
| `capability` | What it can actually do | Docs, changelog, hands-on reviews, benchmarks |
| `pricing` | What it costs, what's in each tier | The vendor's own pricing page — not a blog's number |
| `adoption` | Who uses it, how much | Usage stats, GitHub, surveys, job postings |
| `reception` | What people say about it | Forums, HN, Reddit, app reviews — **look for complaints** |
| `alternatives` | What it competes with, and against which | Comparison pages, migration guides |

Two rules that decide whether the angles pay off:

- **The angles must have different sources.** `pricing` and `basics` both landing on the vendor's
  homepage are one angle wearing two names. If two angles would send an agent to the same places,
  merge them.
- **A subject with one meaningful angle stays one angle.** Do not manufacture parallelism. The
  split is there to get independent evidence, not to look thorough.

### Step 3: check the draft against the current web

Your knowledge has a cutoff and a blind spot. Launch **one** sub-agent (`create_agent`, agent
`researcher`) to check both lists and report:

- subjects missing or wrongly included
- angles that the subject turns out to need (or does not)
- anything that changed recently enough to matter

Give it the topic, today's date, the time range, and the current draft. Today comes from `time` if
available, otherwise `shell` with `date` — a sub-agent cannot see the system prompt's date.

Then merge its findings into the draft and show the user the combined result.

### Step 4: let the user cut it

Show subjects, angles, and the fields each angle will fill. Ask what to drop. Say plainly which
you think is weak — a subject you suspect has too little public information is worth flagging now
rather than discovering when its cell comes back empty.

### Step 5: write the two files

`{topic-slug}/outline.yaml`:

```yaml
topic: AI coding agents
time_range: last 12 months
created: 2026-03-14
subjects:
  - slug: claude-code
    name: Claude Code
    description: Anthropic's terminal agent for software engineering.
  - slug: cursor
    name: Cursor
    description: AI-first IDE built on VS Code.
  - slug: copilot
    name: GitHub Copilot
    description: Microsoft's AI pair-programmer, the incumbent.
cells:
  - subject: claude-code
    angle: pricing
    brief: >
      The pricing Anthropic lists for Claude Code itself, plan by plan. Separate the cost of the
      agent from the cost of the model tokens it consumes — they are billed differently and most
      comparisons conflate them. Note the free tier and any seat minimums.
  - subject: claude-code
    angle: reception
    brief: >
      What developers say after actually using it for a month: where it saves time, where it gets
      in the way, what the setup cost was. Look for specific complaints with specifics, not
      general praise. HN threads and r/ClaudeAI discussions are where this lives.
  - subject: cursor
    angle: pricing
    brief: >
      Cursor's tiers and what separates them, including the usage-based overage model and how
      request limits reset. Note whether the free tier can do real work.
execution:
  max_parallel: 4
  output_dir: results
```

`brief` is the important field: it is the whole reason two cells on the same subject are not the
same research. Write a real brief, not a label. Two or three sentences naming what to establish
and where to look.

`{topic-slug}/fields.yaml` — the schema. Each angle fills **the fields in its category**, not the
whole schema:

```yaml
fields:
  pricing:
    - name: entry_tier
      description: Cheapest paid tier, USD per seat per month.
    - name: consumption_model
      description: How usage beyond the included quota is billed.
  reception:
    - name: praise
      description: What users report working well, with specifics.
    - name: complaints
      description: Recurring complaints, with specifics.
  basics:
    - name: vendor
      description: Organisation behind the product.
    - name: release_date
      description: Public GA date; "[uncertain]" if only a beta date is public.
```

Field names are `snake_case` and become keys in every cell's JSON. `required: true` is optional —
**with no `required` keys anywhere, every field is treated as required**. Values are written in
the user's language; searching may use any language.

**Phase 1 stops here.** State what happens next and wait.

---

## Phase 2 — fan out

### Step 1: find the outline

If the user did not name a topic, `glob` for `*/outline.yaml` under the current directory. More
than one match: ask which. None: go back to phase 1.

### Step 2: skip finished cells

List `results/*/*.json`. A cell whose file exists is done. Do not redo it, and do not overwrite one
without saying why. This is what makes the phase resumable, which it usually needs to be.

### Step 3: dispatch

One sub-agent per cell, `background: true`, at most `execution.max_parallel` at a time. The brief
must be **self-contained** — a sub-agent starts with no memory of this conversation and cannot ask
you anything. Include the subject, the angle, the brief, the absolute path to `fields.yaml`, the
category's field list, and the absolute output path.

```markdown
## Task

Research one cell of a larger investigation and write its JSON.

**Subject**: {subject name} — {subject description}
**Angle**: {angle}

## What this cell must establish

{brief, verbatim}

## Fields you own

Read {absolute path to fields.yaml} and fill **only** the fields in the `{category}` category:

{field list with descriptions}

Other categories belong to other agents working in parallel. Leave them out entirely — a value
you did not verify is worse than an absent key, because nothing downstream can tell them apart.

## Output

Write JSON to {absolute output path}:

- One key per field listed above, exactly as named.
- Every value in {user's language}. Searching may use any language.
- Anything you could not verify: the string "[uncertain]" as the value. Never a plausible guess.
- `"uncertain": ["<field>", ...]` listing every field you marked.
- `"sources"`: the URLs you actually opened. Not the ones the search suggested.
- `"angle": "{angle}"` so the merge step can tell cells apart.

## Before you finish

Re-read your JSON and confirm every field above appears as a key. A missing key is a failure; a
marked-uncertain value is a success.

## Rules

- Prefer primary sources. For a vendor's own product, its own documentation and pricing page beat
  every third-party comparison.
- Give the date of anything time-sensitive. "Latest" is meaningless without it.
- Prefer disagreement over smooth narrative. If two sources conflict, report both and say which is
  more recent — do not quietly pick one.
- If a cell turns out to need much less work than the brief implies, say so in your summary rather
  than padding the JSON.
```

Two things decide whether this fans out well:

- **Absolute paths, always.** A background sub-agent's working directory is not the topic
  directory.
- **Start with one cell, not all of them.** Run the first cell, show the user its JSON, let them
  redirect before spending the rest. Then continue in batches.

### Step 4: verify each cell on arrival

`create_agent` reports completion on its own — do not poll. On completion:

- the file exists and parses
- every field in its category is a key
- missing keys: send the same sub-agent back to fill them, rather than accepting a thin result

### Step 5: report progress honestly

As cells land, say which subjects are done, which are partial, which are empty. A cell that came
back mostly `[uncertain]` is a finding about the subject, not a failure to hide — say so.

---

## Phase 3 — merge

The point of the merge is not to concatenate. It is to see whether a subject's angles **agree**.

### Step 1: merge subject by subject

For each subject, read every JSON in `results/{subject-slug}/` and write
`results/{subject-slug}/merged.md`.

The fields are disjoint by construction, so this is mostly assembly — except where two angles
touched the same fact from different sources. When they do:

- **They agree** → state it once, and note that two independent angles confirmed it.
- **They disagree** → keep both, say what each says and where it came from, and say which is more
  recent. Do not average them and do not quietly drop one. A contradiction between a vendor's
  pricing page and a user's forum post is usually a real and interesting thing.
- **One is `[uncertain]`** → that is not a disagreement; it is a gap. Carry the gap forward.

The gap list is the most valuable output of this step. Write it at the top of `merged.md`.

### Step 2: keep the model in context

Do not hold every cell in context at once. Merge one subject, write its `merged.md`, and let the
previous subject's cells go. The report then reads the `merged.md` files, which are far shorter
than the JSON behind them.

---

## Phase 4 — the report

### Step 1: ask what the reader needs up front

The report opens with a comparison table, and what belongs in it depends on the question from step
1 of phase 1. Offer the fields that actually exist across the merged files — short and comparable
ones. Ask the user to pick. A table of the wrong five columns makes the rest of the report
unreadable.

### Step 2: write `report.md`

Produce it directly. A code generator is not worth it unless the dataset is large enough that
hand-writing would introduce transcription errors.

1. **Title, date researched, time range, scope** — what was covered, as of when, and what was left
   out.
2. **Comparison table** — one row per subject, the user's columns, linking to its section. Mark
   `[不确定]` cells visibly rather than dropping the row's value.
3. **Per-subject sections** — by angle, in the order the user chose, one heading per subject.
4. **Gaps and conflicts** — from the merged files: what nobody could establish, where angles
   disagreed. This section is the reason the two-phase split exists; do not bury it.
5. **Sources** — per subject, the URLs its cells used.

Rules:

- Skip `[不确定]` values in the prose. The table marks them; the narrative does not pad with
  placeholders.
- Lists of objects become a small table or `key: value` lines. A long string becomes a sentence,
  not one enormous bullet.
- **Never invent a number to fill a cell.** An em-dash beats a guess.
- Values and headings in the user's language.

---

## Extending an existing outline

- **A new subject** — append to `subjects:` and add its cells to `cells:`. Only the new cells run
  in phase 2.
- **A new angle for an existing subject** — append its cell. Then re-merge that subject: the point
  of the angle was the second opinion.
- **A new field** — append to its category in `fields.yaml`, and warn. Every existing cell in that
  category no longer satisfies the schema and its subject needs re-running.

Confirm each edit. These files are the contract for a phase that costs real money to run.

---

## Judgement calls

- **A cell that needs no research** — the brief turns out to be answerable from what the user
  already said. Say so and skip it rather than dispatching an agent.
- **An angle that returns nothing** — a subject with no public pricing, no reception, whatever.
  Report the hole; do not substitute a different angle without saying so.
- **Too many cells** — past ~40, the merge step costs more than the research. Suggest cutting
  angles, or splitting the report by subject group.
- **Sources conflict and you cannot tell which is right** — say exactly that. "Two sources from
  2024 and 2025 disagree; neither is authoritative" is a legitimate result.
- **A topic that does not want this** — a single lookup, or a question with one authoritative
  answer, is better served by a direct answer. Load this skill when breadth is the point.
