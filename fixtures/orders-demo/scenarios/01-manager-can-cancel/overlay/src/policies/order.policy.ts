import { Order, User } from "../domain/order";

export class OrderPolicy {
  canCancel(user: User, order: Order): boolean {
    return user.role === "ADMIN" || user.role === "MANAGER";
  }
}
