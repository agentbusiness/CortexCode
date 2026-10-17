/** Project-local wrappers with the shapes of React's `memo` and `forwardRef`. */
export function memo<T>(component: T): T {
  return component;
}

export function forwardRef<T>(render: T): T {
  return render;
}

/** A badge: a component wrapped in a call (its props are its last symbols). */
export const Badge = memo(({ label, tone = "info" }: { label: string; tone?: string }) => <span className={tone}>{label}</span>);

export const Field = forwardRef(function Field({ value }: { value: string }, ref: unknown) {
  return <input ref={ref} value={value} />;
});
