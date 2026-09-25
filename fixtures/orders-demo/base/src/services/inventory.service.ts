import { db } from "../infra/db";
import { OrderItem } from "../domain/order";
import { OutOfStockError } from "../domain/errors";

export class InventoryService {
  async validateInventory(items: OrderItem[]): Promise<void> {
    for (const item of items) {
      const stock = await db.inventory.findUnique({ where: { sku: item.sku } });
      if (!stock || stock.quantity < item.quantity) {
        throw new OutOfStockError(item.sku);
      }
    }
  }

  async reserve(items: OrderItem[]): Promise<void> {
    for (const item of items) {
      await db.inventory.update({ where: { sku: item.sku }, data: { quantity: { decrement: item.quantity } } });
    }
  }

  async release(items: OrderItem[]): Promise<void> {
    for (const item of items) {
      await db.inventory.update({ where: { sku: item.sku }, data: { quantity: { increment: item.quantity } } });
    }
  }
}
