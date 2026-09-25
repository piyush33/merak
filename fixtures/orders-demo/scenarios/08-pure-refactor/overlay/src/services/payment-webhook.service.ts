import { OrderStatus } from "../domain/order";
import { OrderRepository } from "../repositories/orders.repository";

export interface PaymentCaptured {
  orderId: string;
  paymentId: string;
}

export class PaymentWebhookService {
  constructor(private readonly orders: OrderRepository) {}

  async onPaymentCaptured(event: PaymentCaptured): Promise<void> {
    const order = await this.orders.findById(event.orderId);
    if (order.status !== OrderStatus.CONFIRMED) {
      return;
    }
    order.status = OrderStatus.PAID;
    order.paymentId = event.paymentId;
    await this.orders.update(order);
  }
}
