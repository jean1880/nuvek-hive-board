import { ref } from 'vue'

// The API token is never in the bundle: it is typed in once and sent with every request (reads too,
// since the server gates reads by default; a server with HIVE_AUTH_READS=0 ignores it). By default it
// lives in sessionStorage (this tab only). "Remember on this device" is opt-in and moves it to
// localStorage instead. Storage can throw (private windows, blocked site data), so every access is
// wrapped and an in-memory copy backs it up.
const TOKEN_KEY = 'hive_board_token'
const AUTHOR_KEY = 'hive_board_author'

let memToken = ''

function readStored(): string {
  try {
    return sessionStorage.getItem(TOKEN_KEY) || localStorage.getItem(TOKEN_KEY) || ''
  } catch {
    return ''
  }
}

export const hasToken = ref(false)
export const tokenPromptOpen = ref(false)

type Waiter = (token: string | null) => void
let waiters: Waiter[] = []

export function getToken(): string {
  const t = readStored() || memToken
  hasToken.value = t !== ''
  return t
}

function flush(token: string | null) {
  const pending = waiters
  waiters = []
  for (const w of pending) w(token)
}

/** Opens the token modal and resolves with the token, or null if cancelled. */
export function requestToken(): Promise<string | null> {
  tokenPromptOpen.value = true
  return new Promise(resolve => waiters.push(resolve))
}

function clearStored() {
  for (const store of ['sessionStorage', 'localStorage'] as const) {
    try {
      window[store].removeItem(TOKEN_KEY)
    } catch {
      // nothing stored
    }
  }
}

export function submitToken(token: string, remember = false) {
  const t = token.trim()
  if (!t) return
  memToken = t
  clearStored()
  try {
    ;(remember ? localStorage : sessionStorage).setItem(TOKEN_KEY, t)
  } catch {
    // memory copy only
  }
  hasToken.value = true
  tokenPromptOpen.value = false
  flush(t)
}

export function cancelTokenPrompt() {
  tokenPromptOpen.value = false
  flush(null)
}

/** Clears the token from memory, this tab and this device. */
export function forgetToken() {
  memToken = ''
  clearStored()
  hasToken.value = false
}

export function getActor(): string {
  try {
    return localStorage.getItem(AUTHOR_KEY)?.trim() || 'Operator'
  } catch {
    return 'Operator'
  }
}

export function setActor(name: string) {
  const n = name.trim()
  if (!n) return
  try {
    localStorage.setItem(AUTHOR_KEY, n)
  } catch {
    // per-viewer convenience only
  }
}

hasToken.value = getToken() !== ''
