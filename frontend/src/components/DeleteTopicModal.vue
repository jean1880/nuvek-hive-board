<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue'
import { Button, Modal } from '@nuvek/ui'
import type { Topic } from '../types'

const props = defineProps<{
  open: boolean
  topic: Topic | null
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'delete', topic: Topic, confirm: string): void
}>()

const typed = ref('')
const inputId = useId()

watch(
  () => props.open,
  open => {
    if (open) typed.value = ''
  },
)

const matches = computed(() => !!props.topic && typed.value === props.topic.name)

function onSubmit() {
  if (props.topic && matches.value) emit('delete', props.topic, typed.value)
}
</script>

<template>
  <Modal :open="open" title="Delete topic permanently" @close="emit('close')">
    <form v-if="topic" class="space-y-4 pt-1" @submit.prevent="onSubmit">
      <div class="p-3 rounded-lg bg-error/10 border border-error/30 text-xs space-y-1">
        <div class="font-bold text-error">{{ topic.title }}</div>
        <div class="text-fg-muted">
          This removes the topic with its messages, findings, digests and events. It cannot be undone. Resolving or archiving keeps the record instead.
        </div>
      </div>

      <div>
        <label :for="inputId" class="block text-2xs font-mono text-fg-muted mb-1">
          Type <code class="text-fg font-semibold">{{ topic.name }}</code> to confirm
        </label>
        <input
          :id="inputId"
          v-model="typed"
          type="text"
          autocomplete="off"
          spellcheck="false"
          class="w-full text-xs px-3 py-2 rounded-md bg-bg border border-border text-fg font-mono focus:outline-none focus:border-error/60"
        />
      </div>

      <div class="flex items-center justify-end gap-2 pt-2 border-t border-border">
        <Button variant="ghost" type="button" class="text-xs" @click="emit('close')">Cancel</Button>
        <Button variant="danger" type="submit" class="text-xs" :disabled="!matches">Delete topic</Button>
      </div>
    </form>
  </Modal>
</template>
