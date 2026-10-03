import { Marked } from 'marked'
import DOMPurify from 'dompurify'

const escape = (s) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c])

const marked = new Marked({
  gfm: true,
  breaks: false,
  renderer: {
    code({ text, lang }) {
      const label = /^(te?xt|plain(text)?)?$/i.test(lang ?? '') ? '' : escape(lang)
      return `<div class="codeblock"><div class="codeblock-bar"><span>${label}</span><button type="button" data-copy>Copy</button></div><pre><code>${escape(text)}</code></pre></div>`
    },
  },
})

// A link opens a new tab, so clicking one never navigates away from a running session.
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.tagName === 'A' && node.hasAttribute('href')) {
    node.setAttribute('target', '_blank')
    node.setAttribute('rel', 'noopener noreferrer')
  }
})

/** Model output is untrusted: render GFM, then sanitize before it reaches {@html}. */
export function markdown(text) {
  // A form in a reply could send whatever the user types in it to any site.
  return DOMPurify.sanitize(marked.parse(text ?? ''), { FORBID_TAGS: ['form', 'input', 'textarea', 'select'] })
}

/** Delegated handler for the Copy buttons emitted by the code renderer. */
export function copyCode(event) {
  const button = event.target.closest?.('[data-copy]')
  if (!button) return
  navigator.clipboard.writeText(button.closest('.codeblock').querySelector('code').textContent)
  button.textContent = 'Copied'
  setTimeout(() => (button.textContent = 'Copy'), 1200)
}
