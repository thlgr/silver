// Shared turn shaping for the workbench transcript (Turn) and the Messages mode's "Full
// conversation" sheet (Trace). A turn's process groups the same way in both: texts and
// delegations stay standalone, consecutive tool steps collapse together, and a maximal run of
// read-only exploration (member calls interleaved with reasoning or injected steps) collapses
// into one "Explored" group.

import { explores, phrase } from './tools.js'

/** Texts and delegations stay outside the tool groups, so neither is folded away. */
export const isStandalone = (s) => s.kind === 'text' || s.name === 'delegate_task'

/** A turn's final reply: only a trailing text while running, else the newest text, so the
 *  still-running process keeps its chronological order. */
export function finalStep(turn) {
  return turn.running
    ? (turn.steps.at(-1)?.kind === 'text' ? turn.steps.at(-1) : null)
    : turn.steps.findLast((s) => s.kind === 'text')
}

/** Group consecutive non-standalone steps into arrays, then collapse each run of read-only
 *  exploration into one "Explored" group: `[text, tool, tool, text]` → `[text, [tool, tool], text]`
 *  with the tool run further collapsed when it qualifies. */
export function groupParts(steps) {
  const out = []
  for (const step of steps) {
    if (isStandalone(step)) out.push(step)
    else if (Array.isArray(out.at(-1))) out.at(-1).push(step)
    else out.push([step])
  }
  return out.map((part) => (Array.isArray(part) ? groupExplores(part) : part))
}

// A maximal run of read-only calls (member calls interleaved with reasoning or injected steps)
// collapses into one "Explored" group, ending at its last member call. A run of one never
// groups: small models reason between calls, so the rule needs two or more.
function groupExplores(steps) {
  const out = []
  let i = 0
  while (i < steps.length) {
    const step = steps[i]
    if (!explores(step) && step.kind !== 'reasoning' && step.kind !== 'injected') {
      out.push(step)
      i++
      continue
    }
    let j = i
    let lastMember = -1
    let members = 0
    while (j < steps.length && (explores(steps[j]) || steps[j].kind === 'reasoning' || steps[j].kind === 'injected')) {
      if (explores(steps[j])) { lastMember = j; members++ }
      j++
    }
    if (members >= 2) {
      out.push({ kind: 'explore', steps: steps.slice(i, lastMember + 1) })
      i = lastMember + 1
    } else {
      out.push(step)
      i++
    }
  }
  return out
}

/** The steps inside a part, unwrapping an explore group. */
export function flatten(part) {
  return part.flatMap((s) => (s.kind === 'explore' ? s.steps : [s]))
}

/** What a collapsed part says: a finished-process sentence plus failure/denied/hint counts. */
export function summary(part) {
  const steps = flatten(part)
  const tools = steps.filter((s) => s.kind === 'tool')
  const failed = tools.filter((s) => ['failed', 'blocked'].includes(s.status)).length
  const denied = tools.filter((s) => s.status === 'denied').length
  const hints = steps.filter((s) => s.kind === 'advisor' && s.hints.length).length
  const n = tools.length || steps.length
  return { head: phrase(steps) ?? `${n} step${n === 1 ? '' : 's'}`, failed, denied, hints }
}