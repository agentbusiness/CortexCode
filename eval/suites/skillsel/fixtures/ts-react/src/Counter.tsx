import { useEffect, useState } from "react";

interface CounterProps {
  start: number;
  label: string;
}

export function Counter({ start, label }: CounterProps) {
  const [count, setCount] = useState(start);
  const [history, setHistory] = useState<number[]>([]);

  useEffect(() => {
    setHistory([...history, count]);
    setCount(count + 1);
  }, [count, history]);

  return (
    <div>
      <p>
        {label}: {count}
      </p>
      <button onClick={() => setCount(count + 1)}>+1</button>
      <small>{history.length} changes</small>
    </div>
  );
}
