export enum OrderStatus {
  PENDING = "PENDING",
  CONFIRMED = "CONFIRMED",
  PAID = "PAID",
  CANCELLED = "CANCELLED",
}

export interface OrderItem {
  sku: string;
  quantity: number;
}

export interface Order {
  id: string;
  customerId: string;
  status: OrderStatus;
  paymentId: string | null;
  items: OrderItem[];
}

export interface User {
  id: string;
  role: "ADMIN" | "MANAGER" | "CUSTOMER";
}
