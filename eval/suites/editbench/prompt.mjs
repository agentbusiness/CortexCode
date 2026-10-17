// Task wording for the edit-format benchmark. Generated mechanically from the mutation's structural
// type (no judgement about meaning), in the shape of Stencil's published example task.

const ARTICLE = { equality: 'An', comparison: 'A', logical: 'A', arithmetic: 'An' };

export function describeMutation(m) {
  switch (m.type) {
    case 'operator_swap':
      return [`${ARTICLE[m.operator_class] ?? 'An'} ${m.operator_class} operator was swapped for a different one.`,
        `Restore the original ${m.operator_class} operator.`];
    case 'boolean_flip':
      return ['A boolean literal was flipped.', 'Restore the original boolean value.'];
    case 'off_by_one':
      return ['A numeric literal was changed by one (off-by-one error).', 'Restore the original number.'];
    case 'removed_optional_chain':
      return ['An optional chain (`?.`) was replaced with plain property access or call.', 'Restore the missing optional chaining.'];
    case 'removed_guard_clause':
      return [`A guard clause (early ${m.exit_kind}) was removed.`,
        `Restore the missing guard clause (if statement with early ${m.exit_kind}).`];
    case 'renamed_identifier':
      return ['An identifier was renamed, so one reference no longer matches its declaration.',
        'Restore the original identifier name.'];
    default:
      throw new Error(`unknown mutation type ${m.type}`);
  }
}

export function taskPrompt(file, mutation) {
  const [what, how] = describeMutation(mutation);
  return `# Fix the bug in \`${file}\`\n\n${what}\nThe issue is in the \`${mutation.function}\` function.\n${how}\n`;
}
