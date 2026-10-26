<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { Button } from '@nuvek/ui'
import type { Message, MessageKind, PostMessagePayload, Severity } from '../types'
import { renderMarkdown } from '../lib/markdown'
import { getActor, setActor } from '../lib/auth'
import { splitList } from '../lib/format'

const props = defineProps<{
  replyToMessage?: Message | null
  submitting: boolean
}>()

const emit = defineEmits<{
  (e: 'post', payload: PostMessagePayload): void
  (e: 'cancelReply'): void
}>()

// `brief`, `system` and `progress` are written by tooling, not typed by hand.
const kinds: MessageKind[] = ['note', 'finding', 'decision', 'gotcha', 'question', 'verdict']
const severities: Severity[] = ['BLOCKER', 'SHOULD', 'NICE']
const typedKinds: MessageKind[] = ['finding', 'decision', 'gotcha', 'question', 'verdict']

const author = ref('')
const content = ref('')
const kind = ref<MessageKind>('note')
const severity = ref<'' | Severity>('')
const subjectsInput = ref('')
const evidence = ref('')
const tagInput = ref('')
const tags = ref<string[]>([])
const previewMode = ref(false)

onMounted(() => {
  author.value = getActor()
})

watch(author, val => setActor(val))

function addTag() {
  const t = tagInput.value.trim().replace(/^#/, '').toLowerCase()
  if (t && !tags.value.includes(t)) {
    tags.value.push(t)
    tagInput.value = ''
  }
}

function removeTag(tag: string) {
  tags.value = tags.value.filter(t => t !== tag)
}

function handleSubmit() {
  if (!author.value.trim() || !content.value.trim()) return
  const subjects = splitList(subjectsInput.value)
  const payload: PostMessagePayload = {
    author: author.value.trim(),
    content: content.value.trim(),
    parent_id: props.replyToMessage ? props.replyToMessage.id : null,
    tags: [...tags.value],
  }
  if (kind.value !== 'note') payload.kind = kind.value
  if (severity.value) payload.severity = severity.value
  if (subjects.length) payload.subjects = subjects
  if (evidence.value.trim()) payload.evidence = evidence.value.trim()
  emit('post', payload)
  content.value = ''
  evidence.value = ''
  subjectsInput.value = ''
  severity.value = ''
  kind.value = 'note'
  tags.value = []
  previewMode.value = false
}

function handleKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') handleSubmit()
}

const fieldClass =
  'text-xs px-2 py-1 rounded bg-bg border border-border text-fg focus:outline-none focus:border-primary/50'
</script>

<template>
  <div class="border border-border-strong bg-surface/95 rounded-lg p-3.5 space-y-3">
    <div
      v-if="replyToMessage"
      class="flex items-center justify-between p-2 rounded bg-primary/10 border border-primary/20 text-xs"
    >
      <div class="flex items-center gap-1.5 truncate">
        <span class="text-primary font-bold">Replying to #{{ replyToMessage.id }} ({{ replyToMessage.author }}):</span>
        <span class="text-fg-muted truncate">{{ replyToMessage.content.substring(0, 60) }}...</span>
      </div>
      <button type="button" class="text-xs text-fg-dim hover:text-fg px-1.5 py-0.5" @click="emit('cancelReply')">
        ✕ Cancel
      </button>
    </div>

    <div class="flex items-center justify-between gap-3 flex-wrap">
      <div class="flex items-center gap-3 flex-wrap">
        <label class="flex items-center gap-2 text-2xs font-mono text-fg-dim">
          Author
          <input v-model="author" type="text" placeholder="Agent or user name" :class="[fieldClass, 'w-36 sm:w-44 font-medium']" />
        </label>
        <label class="flex items-center gap-2 text-2xs font-mono text-fg-dim">
          Kind
          <select v-model="kind" :class="[fieldClass, 'font-mono']">
            <option v-for="k in kinds" :key="k" :value="k">{{ k }}</option>
          </select>
        </label>
        <label class="flex items-center gap-2 text-2xs font-mono text-fg-dim">
          Severity
          <select v-model="severity" :class="[fieldClass, 'font-mono']">
            <option value="">none</option>
            <option v-for="s in severities" :key="s" :value="s">{{ s }}</option>
          </select>
        </label>
      </div>

      <div class="flex items-center gap-1 bg-bg-alt p-0.5 rounded border border-border text-2xs">
        <button
          type="button"
          :class="['px-2 py-0.5 rounded cursor-pointer', !previewMode ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
          @click="previewMode = false"
        >
          Write
        </button>
        <button
          type="button"
          :class="['px-2 py-0.5 rounded cursor-pointer', previewMode ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
          @click="previewMode = true"
        >
          Preview
        </button>
      </div>
    </div>

    <p v-if="typedKinds.includes(kind)" class="text-2xs text-fg-muted">
      A {{ kind }} is also recorded as a finding that can be confirmed or dismissed later.
    </p>

    <div v-if="!previewMode">
      <textarea
        v-model="content"
        rows="4"
        aria-label="Message body"
        placeholder="Write the message in Markdown... (Ctrl+Enter to post)"
        class="w-full text-xs sm:text-sm p-3 rounded-md bg-bg border border-border text-fg placeholder:text-fg-dim focus:outline-none focus:border-primary/50 resize-y font-mono"
        @keydown="handleKeydown"
      ></textarea>
    </div>
    <div
      v-else
      class="min-h-24 p-3 rounded-md bg-bg border border-border text-fg text-xs sm:text-sm markdown-body overflow-x-auto"
      v-html="renderMarkdown(content || '*Nothing to preview*')"
    ></div>

    <div class="grid grid-cols-1 sm:grid-cols-2 gap-2">
      <label class="block text-2xs font-mono text-fg-dim">
        Subjects (comma-separated)
        <input v-model="subjectsInput" type="text" placeholder="hive-board, nginx" :class="[fieldClass, 'w-full mt-1 font-mono']" />
      </label>
      <label class="block text-2xs font-mono text-fg-dim">
        Evidence (optional)
        <input v-model="evidence" type="text" placeholder="command output, file:line, URL" :class="[fieldClass, 'w-full mt-1 font-mono']" />
      </label>
    </div>

    <div class="flex flex-col sm:flex-row items-start sm:items-center justify-between gap-2.5 pt-1 border-t border-border/50">
      <div class="flex items-center gap-1.5 flex-wrap">
        <span class="text-2xs font-mono text-fg-dim">Tags:</span>
        <span
          v-for="t in tags"
          :key="t"
          class="inline-flex items-center gap-1 text-2xs font-mono px-1.5 py-0.5 rounded bg-bg border border-border text-fg-muted"
        >
          #{{ t }}
          <button type="button" class="hover:text-error" :aria-label="`Remove tag ${t}`" @click="removeTag(t)">×</button>
        </span>
        <input
          v-model="tagInput"
          type="text"
          aria-label="Add tag"
          placeholder="+ tag"
          :class="[fieldClass, 'w-16 font-mono py-0.5']"
          @keydown.enter.prevent="addTag"
          @keydown.space.prevent="addTag"
        />
      </div>

      <div class="flex items-center gap-2 self-end sm:self-auto">
        <span class="text-2xs text-fg-dim font-mono hidden sm:inline">Ctrl + Enter</span>
        <Button variant="primary" class="text-xs px-3.5 py-1.5" :disabled="!content.trim() || !author.trim() || submitting" @click="handleSubmit">
          <span v-if="submitting">Posting...</span>
          <span v-else>{{ replyToMessage ? 'Post reply' : 'Post message' }}</span>
        </Button>
      </div>
    </div>
  </div>
</template>
