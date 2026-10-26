<script setup lang="ts">
import { Button, Modal } from '@nuvek/ui'
import type { Topic } from '../types'

const props = defineProps<{
  open: boolean
  topic: Topic | null
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'distill', topic: string): void
}>()

function handleDistill() {
  if (props.topic) emit('distill', props.topic.name)
}
</script>

<template>
  <Modal :open="open" title="Distill now (keyword fallback)" @close="emit('close')">
    <div v-if="topic" class="space-y-4 pt-1">
      <div class="p-3 rounded-lg bg-primary/10 border border-primary/20 text-xs space-y-1">
        <div class="font-bold text-primary">{{ topic.title }}</div>
        <div class="text-fg-muted font-mono text-2xs">{{ topic.message_count }} messages · {{ topic.open_findings }} open findings</div>
      </div>

      <p class="text-xs text-fg-muted leading-relaxed">
        The gardener produces the full digest automatically: it summarises the thread, extracts findings and links them to the knowledge graph whenever a topic goes quiet.
      </p>
      <p class="text-xs text-fg-muted leading-relaxed">
        This button writes a quick keyword digest right now instead. Nothing is deleted: every message stays on the board, and the gardener can still write a fuller digest later.
      </p>

      <div class="flex items-center justify-end gap-2 pt-3 border-t border-border">
        <Button variant="ghost" type="button" class="text-xs" @click="emit('close')">Cancel</Button>
        <Button variant="primary" type="button" class="text-xs" @click="handleDistill">Distill now</Button>
      </div>
    </div>
  </Modal>
</template>
