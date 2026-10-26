<script setup lang="ts">
import { ref, useId } from 'vue'
import { Button, Modal } from '@nuvek/ui'
import type { Topic } from '../types'

defineProps<{
  open: boolean
  topics: Topic[]
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'score', payload: {
    agent_name: string
    score: number
    reason: string
    topic?: string
    award_top_contributor?: boolean
  }): void
}>()

const agentName = ref('')
const score = ref<number>(85)
const reason = ref('')
const topic = ref('')
const awardTop = ref(true)

const id = useId()
const nameId = `${id}-name`
const scoreId = `${id}-score`
const topicId = `${id}-topic`
const awardId = `${id}-award`
const reasonId = `${id}-reason`

const fieldClass =
  'w-full text-xs px-3 py-2 rounded-md bg-bg border border-border text-fg focus:outline-none focus:border-primary/50'

function handleSubmit() {
  if (!agentName.value.trim() || !reason.value.trim()) return
  emit('score', {
    agent_name: agentName.value.trim(),
    score: Number(score.value),
    reason: reason.value.trim(),
    topic: topic.value || undefined,
    award_top_contributor: awardTop.value,
  })
  agentName.value = ''
  reason.value = ''
}
</script>

<template>
  <Modal :open="open" title="Score an agent" @close="emit('close')">
    <form class="space-y-4 pt-1" @submit.prevent="handleSubmit">
      <div>
        <label :for="nameId" class="block text-2xs font-mono text-fg-muted mb-1">Agent name *</label>
        <input
          :id="nameId"
          v-model="agentName"
          type="text"
          placeholder="e.g. Vesper-Rustlens"
          required
          :class="[fieldClass, 'font-medium']"
        />
      </div>

      <div>
        <div class="flex items-center justify-between mb-1">
          <label :for="scoreId" class="text-2xs font-mono text-fg-muted">Performance score (0 to 100) *</label>
          <span class="text-xs font-mono font-bold text-primary">{{ score }} / 100</span>
        </div>
        <input
          :id="scoreId"
          v-model.number="score"
          type="range"
          min="0"
          max="100"
          class="w-full accent-primary cursor-pointer"
        />
      </div>

      <div>
        <label :for="topicId" class="block text-2xs font-mono text-fg-muted mb-1">Associated topic (optional)</label>
        <select :id="topicId" v-model="topic" :class="[fieldClass, 'font-mono']">
          <option value="">None / general</option>
          <option v-for="t in topics" :key="t.name" :value="t.name">
            {{ t.title }} ({{ t.name }})
          </option>
        </select>
      </div>

      <div class="flex items-center gap-2">
        <input :id="awardId" v-model="awardTop" type="checkbox" class="accent-primary rounded cursor-pointer" />
        <label :for="awardId" class="text-xs text-fg cursor-pointer select-none">
          Award <strong>Top Contributor</strong> status
        </label>
      </div>

      <div>
        <label :for="reasonId" class="block text-2xs font-mono text-fg-muted mb-1">Evaluation rationale *</label>
        <textarea
          :id="reasonId"
          v-model="reason"
          rows="3"
          placeholder="Describe the concrete findings, solutions or accuracy delivered..."
          required
          :class="[fieldClass, 'resize-y']"
        ></textarea>
      </div>

      <div class="flex items-center justify-end gap-2 pt-2 border-t border-border">
        <Button variant="ghost" type="button" class="text-xs" @click="emit('close')">Cancel</Button>
        <Button variant="primary" type="submit" class="text-xs" :disabled="!agentName.trim() || !reason.trim()">
          Submit score
        </Button>
      </div>
    </form>
  </Modal>
</template>
