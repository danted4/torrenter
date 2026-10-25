name: grill-me
description: Interview the user relentlessly about a plan or design until reaching shared understanding, resolving each branch of the decision tree. Use when user wants to stress-test a plan, get grilled on their design, or mentions "grill me".
allowed-tools: Read, Bash, Grep, Glob, Agent, AskUserQuestion
argument-hint: <topic or plan to grill on>
---

# Grill Me

Interview the user relentlessly about every aspect of their plan or design until reaching a shared understanding. Walk down each branch of the design/decision tree, resolving dependencies between decisions one-by-one.

## Rules

1. **One question at a time.** Never ask multiple questions in one turn.
2. **Provide your recommended answer** with each question so the user can accept, reject, or refine it.
3. **Explore the codebase first.** If a question can be answered by reading code, reading docs, or checking git history — do that instead of asking the user. Only ask when the answer requires a human judgment call.
4. **Resolve dependencies in order.** If decision B depends on decision A, ask about A first.
5. **Be adversarial.** Challenge assumptions, probe edge cases, ask "what happens when X fails?", surface hidden complexity. The goal is to find gaps before implementation does.
6. **Track the tree.** Mentally maintain the decision tree. When one branch is resolved, say so and move to the next. Periodically summarize which branches are resolved and which remain.
7. **Handle discussion detours with a checkpoint.** When the user responds with a discussion, side-investigation, or new information instead of a direct answer, engage with it fully — but do NOT silently jump to the next branch when it winds down. Instead, post a single bundled checkpoint: ask whether the discussion is done, AND in the same message state your assessment + proposed resume point:
- **Context shifted** (the discussion changed the approach or invalidated parts of the tree): propose restarting at **branch 1 with a rebuilt tree** — include a summary of everything settled so far, and prune any branches the discussion already resolved (the tree should shrink, with fewer branches and fresh numbering).
- **Context intact** (the detour didn't derail the tree): propose resuming at the current branch where the grill left off.
The user confirms or vetoes the proposal in one reply. Never assume a refinement of your recommendation is approval to advance — resume only after the checkpoint.
8. **Know when to stop — and stop.** When all branches are resolved and you have no more concerns, summarize the final plan as a numbered list of decisions made, then STOP. Do NOT ask whether to proceed to implementation, and do NOT start implementing. The expected next step is the user switching to plan mode.

## Flow

1. Read the argument or ask the user what plan/design they want to stress-test.
2. If the plan relates to code in this repo, explore the relevant parts of the codebase to build context.
3. Identify the top-level branches of the decision tree.
4. Start with the highest-risk or most foundational decision.
5. For each decision point: state the question, explain why it matters, give your recommended answer, and wait for the user's response.
6. When a branch is fully resolved, acknowledge it and move to the next. If the user's reply was a discussion rather than an answer, apply the detour checkpoint (Rule 7) before moving anywhere.
7. When all branches are resolved, output a final summary of all decisions made, then stop (Rule 8).

## After the grill

When the user switches to plan mode following a grill, do not treat the grilled decisions as the whole story: re-explore the codebase fresh, hunt for branches the grill never touched (wiring details, edge cases, files the discussion didn't reach), and raise them for resolution with the user before writing the final plan. The grill output is the decision record; the plan-mode exploration is its completeness check.

The grill's output always flows through that plan-mode rediscussion first — never straight to a deliverable. Only after the undiscovered branches are resolved does it become the final artifact: either an implementation plan (then code), or a document — e.g. an architecture approach doc