import { UserCard } from './components/UserCard';
import { placeOrder, healthy } from './api';
import { loadSettings } from './ipc';

export default function App() {
  const onBuy = () => placeOrder(['book'], 'SPRING');
  healthy();
  loadSettings('theme');
  return <UserCard userId={7} compact />;
}
