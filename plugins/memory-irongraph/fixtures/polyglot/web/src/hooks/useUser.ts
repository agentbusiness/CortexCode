import { useEffect, useState } from 'react';
import { fetchUser, User } from '../api';

export function useUser(id: number) {
  const [user, setUser] = useState<User | null>(null);
  useEffect(() => {
    fetchUser(id, true).then(setUser);
  }, [id]);
  return user;
}
