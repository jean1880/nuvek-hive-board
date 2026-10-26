<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue'
import { Button, Modal } from '@nuvek/ui'
import { TOPIC_NAME_RE, type TopicKind } from '../types'
import { getActor } from '../lib/auth'

defineProps<{
  open: boolean
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'create', payload: { name: string; title: string; description?: string; author: string; kind: TopicKind }): void
}>()

const kinds: TopicKind[] = ['task', 'incident', 'report', 'design', 'review', 'research']

const title = ref('')
const name = ref('')
const author = ref(getActor())
const kind = ref<TopicKind>('task')
const description = ref('')

const id = useId()
const titleId = `${id}-title`
const nameId = `${id}-name`
const nameErrId = `${id}-name-err`
const kindId = `${id}-kind`
const authorId = `${id}-author`
const descId = `${id}-desc`

const fieldClass =
  'w-full text-xs px-3 py-2 rounded-md bg-bg border border-border text-fg focus:outline-none focus:border-primary/50'

const nameValid = computed(() => TOPIC_NAME_RE.test(name.value.trim()))

watch(title, (val) => {
  if (!name.value || name.value === slugify(val.slice(0, -1))) {
    name.value = slugify(val)
  }
})

// Server rule: ^[a-z][a-z0-9-]{1,79}$
function slugify(text: string): string {
  return text
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9\s-]/g, '')
    .replace(/[\s_-]+/g, '-')
    .replace(/^[^a-z]+/, '')
    .slice(0, 80)
    .replace(/-+$/g, '')
}

function handleCreate() {
  if (!title.value.trim() || !nameValid.value || !author.value.trim()) return
  emit('create', {
    title: title.value.trim(),
    name: name.value.trim(),
    author: author.value.trim(),
    kind: kind.value,
    description: description.value.trim() || undefined,
  })
  title.value = ''
  name.value = ''
  description.value = ''
}
</script>

<template>
  <Modal :open="open" title="Create a topic" @close="emit('close')">
    <form class="space-y-4 pt-1" @submit.prevent="handleCreate">
      <div>
        <label :for="titleId" class="block text-2xs font-mono text-fg-muted mb-1">Topic title *</label>
        <input
          :id="titleId"
          v-model="title"
          type="text"
          placeholder="e.g. agent-terminal 3.0: finishing pass"
          required
          :class="fieldClass"
        />
      </div>

      <div>
        <label :for="nameId" class="block text-2xs font-mono text-fg-muted mb-1">Slug / identifier *</label>
        <input
          :id="nameId"
          v-model="name"
          type="text"
          placeholder="e.g. agent-terminal-v3-finish"
          required
          :aria-invalid="name && !nameValid ? 'true' : undefined"
          :aria-describedby="name && !nameValid ? nameErrId : undefined"
          :class="[fieldClass, 'font-mono']"
        />
        <p v-if="name && !nameValid" :id="nameErrId" class="text-2xs text-error mt-1" role="alert">
          Use 2 to 80 characters: lowercase letters, digits and hyphens, starting with a letter.
        </p>
      </div>

      <div>
        <label :for="kindId" class="block text-2xs font-mono text-fg-muted mb-1">Kind</label>
        <select :id="kindId" v-model="kind" :class="[fieldClass, 'font-mono']">
          <option v-for="k in kinds" :key="k" :value="k">{{ k }}</option>
        </select>
      </div>

      <div>
        <label :for="authorId" class="block text-2xs font-mono text-fg-muted mb-1">Author / lead agent *</label>
        <input
          :id="authorId"
          v-model="author"
          type="text"
          placeholder="e.g. Orchestrator or Operator"
          required
          :class="fieldClass"
        />
      </div>

      <div>
        <label :for="descId" class="block text-2xs font-mono text-fg-muted mb-1">Mission / description (optional)</label>
        <textarea
          :id="descId"
          v-model="description"
          rows="3"
          placeholder="Mission scope, branch, deliverables, constraints..."
          :class="[fieldClass, 'resize-y']"
        ></textarea>
      </div>

      <div class="flex items-center justify-end gap-2 pt-2 border-t border-border">
        <Button variant="ghost" type="button" class="text-xs" @click="emit('close')">Cancel</Button>
        <Button variant="primary" type="submit" class="text-xs" :disabled="!title.trim() || !nameValid || !author.trim()">
          Create topic
        </Button>
      </div>
    </form>
  </Modal>
</template>
