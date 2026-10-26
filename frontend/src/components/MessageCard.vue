<script setup lang="ts">
import { computed } from 'vue'
import { Button, Chip } from '@nuvek/ui'
import type { Message } from '../types'
import { renderMarkdown } from '../lib/markdown'
import { formatDate, messageKindColor, severityColor } from '../lib/format'
import OverflowMenu from './OverflowMenu.vue'

const props = defineProps<{
  message: Message
  isReply?: boolean
}>()

const emit = defineEmits<{
  (e: 'reply', message: Message): void
  (e: 'delete', message: Message): void
}>()

const identity = computed(() => props.message.identity)

const renderedHtml = computed(() => renderMarkdown(props.message.content))
</script>

<template>
  <div
    :id="`message-${message.id}`"
    :class="[
      'rounded-lg border bg-surface/90 transition-all group',
      message.kind === 'system' ? 'border-dashed' : '',
      isReply ? 'ml-4 sm:ml-8 border-l-2 border-l-primary/40 border-border/80' : 'border-border/80',
    ]"
  >
    <div class="px-3.5 py-2.5 border-b border-border/60 flex items-center justify-between gap-3 bg-surface-hover/30">
      <div class="flex items-center gap-2.5 min-w-0">
        <div
          class="w-7 h-7 rounded-full flex items-center justify-center text-xs font-bold shrink-0 border border-primary/30 bg-primary/15 text-primary uppercase font-mono"
          aria-hidden="true"
        >
          {{ message.author.substring(0, 2) }}
        </div>

        <div class="flex items-center gap-1.5 flex-wrap min-w-0">
          <span class="text-xs font-bold text-fg truncate">{{ message.author }}</span>
          <span
            v-if="identity"
            class="text-2xs font-mono text-fg-dim"
            :title="`${identity.agent_type} / ${identity.model} / ${identity.harness}`"
          >
            {{ identity.agent_type }}
          </span>
          <span class="text-2xs text-fg-dim font-mono">#{{ message.id }}</span>
        </div>
      </div>

      <div class="flex items-center gap-2 shrink-0">
        <span class="text-2xs text-fg-dim font-mono" :title="message.created_at">
          {{ formatDate(message.created_at) }}
        </span>
        <Button variant="ghost" class="text-2xs px-2 py-0.5 normal-case" @click="emit('reply', message)">
          Reply
        </Button>
        <OverflowMenu
          label="More message actions"
          :items="[{ key: 'delete', label: 'Delete message…', danger: true }]"
          @select="emit('delete', message)"
        />
      </div>
    </div>

    <div class="px-3.5 pt-2 flex flex-wrap items-center gap-1">
      <Chip
        v-if="!message.evidence && ['finding', 'gotcha', 'verdict'].includes(message.kind)"
        color="warning"
        variant="outlined"
        size="sm"
        title="No command output or file:line was attached"
      >
        no evidence
      </Chip>
      <Chip v-if="message.kind !== 'note'" :color="messageKindColor(message.kind)" size="sm">{{ message.kind }}</Chip>
      <Chip v-if="message.severity" :color="severityColor[message.severity]" variant="filled" size="sm">
        {{ message.severity }}
      </Chip>
      <Chip v-if="message.ephemeral" color="idle" variant="outlined" size="sm">ephemeral</Chip>
      <Chip v-if="message.compacted_at" color="idle" variant="outlined" size="sm">compacted</Chip>
      <span
        v-for="s in message.subjects"
        :key="`s-${s}`"
        class="text-2xs font-mono px-1.5 py-0.5 rounded bg-primary/10 border border-primary/25 text-primary"
        title="Subject"
      >
        {{ s }}
      </span>
      <span
        v-for="tag in message.tags"
        :key="`t-${tag}`"
        class="text-2xs font-mono px-1.5 py-0.5 rounded bg-bg border border-border text-fg-muted"
      >
        #{{ tag }}
      </span>
    </div>

    <div class="px-3.5 py-3 text-fg text-xs sm:text-sm markdown-body overflow-x-auto" v-html="renderedHtml"></div>

    <div v-if="message.evidence" class="mx-3.5 mb-3 rounded-md border border-border bg-bg p-2.5">
      <div class="text-2xs font-mono uppercase text-fg-dim mb-1">Evidence</div>
      <pre class="text-2xs font-mono text-fg-muted whitespace-pre-wrap break-words">{{ message.evidence }}</pre>
    </div>
  </div>
</template>
