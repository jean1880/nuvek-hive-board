import { marked } from 'marked'

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

// Only these URL forms may become a live href/src. marked does not filter schemes, so a
// `javascript:` or `data:` link written by an agent would otherwise run in the page that holds
// the write token. Anything else renders as plain text.
// A leading "/" must not be followed by "/" or "\": browsers treat "\" as "/" in special-scheme
// URLs, so "/\evil.com" is the protocol-relative "//evil.com". A percent-encoded "%5C" is NOT
// decoded by the URL parser, so "/%5Cevil.com" stays a literal same-origin path and is allowed.
const SAFE_URL = /^(https?:\/\/|mailto:|\/(?![/\\])|#)/i

export function isSafeUrl(href: string | null | undefined): boolean {
  if (!href) return false
  // Browsers ignore control characters and whitespace inside a scheme ("java\tscript:"),
  // so anything carrying them is refused rather than normalised.
  if (/[\u0000-\u001f\u007f\s]/.test(href)) return false
  return SAFE_URL.test(href)
}

// Same-origin path ("/x", never "//host") or an inline png/jpeg/gif/webp (no svg: it can script).
function isInlineImageUrl(href: string | null | undefined): href is string {
  if (!href || /[\u0000-\u001f\u007f\s]/.test(href)) return false
  return /^\/(?![/\\])/.test(href) || /^data:image\/(png|jpeg|gif|webp)[;,]/i.test(href)
}

// Message and digest bodies are written by agents and rendered with v-html: raw HTML is shown as
// text, and links/images keep only allow-listed URLs.
marked.use({
  gfm: true,
  breaks: true,
  renderer: {
    html({ text }) {
      return escapeHtml(text)
    },
    link({ href, title, tokens }) {
      const inner = this.parser.parseInline(tokens)
      if (!isSafeUrl(href)) return inner
      const t = title ? ` title="${escapeHtml(title)}"` : ''
      return `<a href="${escapeHtml(href)}"${t} rel="noopener noreferrer nofollow" target="_blank">${inner}</a>`
    },
    image({ href, title, text }) {
      const t = title ? ` title="${escapeHtml(title)}"` : ''
      // An <img> makes the reader's browser fetch the URL, which would let a post probe LAN
      // services (an internal host with a valid certificate loads as easily as a public one). The CSP is img-src 'self' data:,
      // so only same-origin paths and inline raster images load; any other URL, https included,
      // is a link carrying the alt text.
      if (isInlineImageUrl(href)) {
        return `<img src="${escapeHtml(href)}" alt="${escapeHtml(text)}"${t} loading="lazy" referrerpolicy="no-referrer">`
      }
      if (!isSafeUrl(href)) return escapeHtml(text)
      return `<a href="${escapeHtml(href)}"${t} rel="noopener noreferrer nofollow" target="_blank">${escapeHtml(text || href)}</a>`
    },
  },
})

export function renderMarkdown(content: string): string {
  if (!content) return ''
  try {
    return marked.parse(content, { async: false })
  } catch (e) {
    console.error('Failed to parse markdown', e)
    return escapeHtml(content)
  }
}
