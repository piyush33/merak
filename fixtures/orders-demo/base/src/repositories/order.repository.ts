import { db } from "../infra/db";
import { Order } from "../domain/order";

export class OrderRepository {
  async findById(id: string): Promise<Order> {
    return db.order.findUnique({ where: { id } });
  }

  async insert(order: Order): Promise<void> {
    await db.order.create({ data: order });
  }

  async update(order: Order): Promise<void> {
    await db.order.update({ where: { id: order.id }, data: order });
  }
}
