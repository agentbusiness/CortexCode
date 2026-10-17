import { Avatar } from './Avatar';
import { useUser } from '../hooks/useUser';

export function Badge({ label, tone = 'neutral' }: { label: string; tone?: string }) {
  return <span className={tone}>{label}</span>;
}

export function UserCard({ userId, compact = false }: { userId: number; compact?: boolean }) {
  const user = useUser(userId);
  if (!user) return null;
  return (
    <div>
      <Avatar url={user.avatar} size={compact ? 24 : 48} />
      <Badge label={user.role} />
    </div>
  );
}
