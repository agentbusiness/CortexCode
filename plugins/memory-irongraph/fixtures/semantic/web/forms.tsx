import { useState } from 'react';

/** A text box that shows how many characters remain before the limit. */
export function CharacterLimitedInput({ max = 140 }: { max?: number }) {
  const [value, setValue] = useState('');
  return (
    <label>
      <input value={value} onChange={(e) => setValue(e.target.value.slice(0, max))} />
      <span>{max - value.length}</span>
    </label>
  );
}

/** A star rating widget where clicking a star sets the score. */
export function StarRating({ stars = 5 }: { stars?: number }) {
  const [score, setScore] = useState(0);
  return <div>{Array.from({ length: stars }, (_, i) => <button key={i} onClick={() => setScore(i + 1)}>{i < score ? '★' : '☆'}</button>)}</div>;
}

/** Keeps a boolean flag and a function that flips it. */
export function useToggle(initial = false): [boolean, () => void] {
  const [on, setOn] = useState(initial);
  return [on, () => setOn((v) => !v)];
}
