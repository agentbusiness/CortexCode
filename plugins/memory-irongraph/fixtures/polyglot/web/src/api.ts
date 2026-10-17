import axios from 'axios';
import { formatMoney } from 'tiny-format';

export interface User {
  id: number;
  name: string;
  avatar: string;
  role: string;
}

export class BaseError {
  constructor(public message: string) {}
}

/** An HTTP failure with its status code. */
export class ApiError extends BaseError {
  status: number;
  constructor(status: number) {
    super('request failed');
    this.status = status;
  }
}

/** Load one user record from the backend. */
export async function fetchUser(id: number, verbose = false): Promise<User> {
  const response = await fetch(`/api/users/${id}?verbose=${verbose}`);
  if (!response.ok) throw new ApiError(response.status);
  return response.json();
}

export async function placeOrder(items: string[], coupon?: string) {
  const result = await axios.post('/api/orders', { items, coupon });
  return formatMoney(result.data.total, 'EUR');
}

export async function healthy(): Promise<boolean> {
  const response = await fetch('http://localhost:5000/api/health');
  return response.ok;
}
