// Run with `npm test` (node --test; Node strips the TypeScript types natively).
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { isSafeUrl, renderMarkdown } from './markdown.ts'

test('script-bearing links render as text', () => {
  for (const md of [
    '[x](javascript:alert(1))',
    '[x](JaVaScRiPt:alert(1))',
    '[x](data:text/html;base64,PHNjcmlwdD4=)',
    '[x](vbscript:msgbox)',
    '[x](//evil.example/path)',
  ]) {
    const html = renderMarkdown(md)
    assert.ok(!html.includes('href='), `${md} -> ${html}`)
  }
})

test('script-bearing images render as alt text', () => {
  const html = renderMarkdown('![pic](javascript:alert(1))')
  assert.ok(!html.includes('<img'), html)
})

test('only same-origin and inline raster images load', () => {
  for (const md of ['![a](/local.png)', '![a](data:image/png;base64,iVBORw0KGgo=)', '![a](data:image/webp;base64,AAAA)']) {
    assert.ok(renderMarkdown(md).includes('<img src='), md)
  }
})

test('every other image URL becomes a link with the alt text', () => {
  for (const md of [
    '![shot](https://graph.example.test/x.png)',
    '![shot](http://192.0.2.1/x.png)',
    '![shot](//evil.example/x.png)',
    '![shot](mailto:a@b.c)',
  ]) {
    const html = renderMarkdown(md)
    assert.ok(!html.includes('<img'), `${md} -> ${html}`)
    if (md.includes('//evil')) assert.ok(!html.includes('href='), `${md} -> ${html}`)
    else assert.ok(html.includes('>shot</a>'), `${md} -> ${html}`)
  }
})

test('a backslash after the leading slash is protocol-relative and refused', () => {
  for (const md of ['[x](/\\evil.com)', '![x](/\\evil.com/a.png)', '[x](/\\/evil.com)']) {
    const html = renderMarkdown(md)
    assert.ok(!html.includes('href=') && !html.includes('<img'), `${md} -> ${html}`)
  }
  assert.equal(isSafeUrl('/\\evil.com'), false)
})

test('a percent-encoded backslash stays a same-origin path', () => {
  // The URL parser does not decode %5C, so this resolves on our own origin.
  assert.equal(isSafeUrl('/%5Cevil.com'), true)
  assert.ok(renderMarkdown('[x](/%5Cevil.com)').includes('href="/%5Cevil.com"'))
  assert.ok(renderMarkdown('![x](/%5Cevil.com/a.png)').includes('<img src="/%5Cevil.com/a.png"'))
})

test('svg and other data images never load', () => {
  for (const md of ['![a](data:image/svg+xml;base64,PHN2Zz4=)', '![a](data:text/html;base64,AAAA)']) {
    const html = renderMarkdown(md)
    assert.ok(!html.includes('<img') && !html.includes('href='), `${md} -> ${html}`)
  }
})

test('an http image alt text is escaped inside its link', () => {
  const html = renderMarkdown('![<b>x</b>](http://example.com/x.png)')
  assert.ok(!html.includes('<b>'), html)
})

test('raw html is escaped', () => {
  const html = renderMarkdown('<img src=x onerror=alert(1)>')
  assert.ok(!html.includes('<img'), html)
})

test('attribute injection through a title is escaped', () => {
  const html = renderMarkdown('[x](https://example.com "a\\" onmouseover=\\"alert(1)")')
  assert.ok(!/" onmouseover=/.test(html), html)
})

test('safe links keep working', () => {
  const html = renderMarkdown('[docs](https://example.com/a?b=1) and [top](#t) and [rel](/x)')
  assert.ok(html.includes('href="https://example.com/a?b=1"'), html)
  assert.ok(html.includes('href="#t"'), html)
  assert.ok(html.includes('href="/x"'), html)
})

test('control characters inside a scheme are refused', () => {
  assert.equal(isSafeUrl('java\tscript:alert(1)'), false)
  assert.equal(isSafeUrl('https://ok.example'), true)
})
