import axios from "axios";
import { Order } from "../domain/order";

export async function notifyWarehouse(order: Order): Promise<void> {
  await axios.post("https://warehouse.example.com/release", { orderId: order.id, items: order.items });
}
